//! Bounded JSON framing and authenticated loopback owner requests.
use super::*;
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;

pub(super) fn write_frame(writer: &mut impl Write, value: &impl Serialize) -> Result<(), Fault> {
    let mut bytes = serde_json::to_vec(value).map_err(local)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).map_err(io_fault)
}

pub(super) fn read_frame<T: serde::de::DeserializeOwned>(reader: impl Read) -> Result<T, Fault> {
    let mut line = Vec::new();
    BufReader::new(reader.take(MAX_FRAME + 1))
        .read_until(b'\n', &mut line)
        .map_err(io_fault)?;
    if line.len() as u64 > MAX_FRAME || line.last() != Some(&b'\n') {
        return Err(local("incomplete or oversized local protocol frame"));
    }
    serde_json::from_slice(&line).map_err(local)
}

pub(super) fn request(entry: &Entry, action: Action, timeout: u64) -> Result<Reply, Fault> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, entry.port));
    let mut stream =
        TcpStream::connect_timeout(&addr, Duration::from_secs(timeout)).map_err(|e| {
            if e.kind() == std::io::ErrorKind::ConnectionRefused {
                stopped_owner()
            } else {
                local(e)
            }
        })?;
    // The owner needs its entire wait budget, plus time to serialize and return
    // the final screen. A transport deadline at the same instant loses that reply.
    let input_mode = input_mode(&action);
    let response_timeout = timeout
        + if matches!(input_mode, Some((false, _))) {
            2
        } else {
            0
        };
    stream
        .set_read_timeout(Some(Duration::from_secs(response_timeout)))
        .map_err(local)?;
    stream
        .set_write_timeout(Some(Duration::from_secs(timeout)))
        .map_err(local)?;
    let mutation = matches!(
        action,
        Action::Send { .. } | Action::Keys { .. } | Action::Resize { .. } | Action::Close { .. }
    ) || expected_id(&action).is_some();
    let snapshot = matches!(
        action,
        Action::Read { .. } | Action::Send { .. } | Action::Keys { .. }
    );
    let result = (|| {
        write_frame(
            &mut stream,
            &Request {
                token: entry.token.clone(),
                action,
                timeout,
            },
        )?;
        let mut reply: Reply = read_frame(stream)?;
        // Existing owners ignore unknown request fields and return complete grids.
        // Do not mislabel their reply as an incremental read.
        if snapshot && reply.screen_mode.is_none() && reply.screen.is_some() {
            reply.screen_mode = Some("full".into());
        }
        if let Some((async_mode, _)) = input_mode {
            if reply.wait_status.is_none() && reply.error.is_none() {
                reply.wait_status = Some(if async_mode { "skipped" } else { "unsupported" }.into());
            }
        }
        Ok(reply)
    })();
    result.map_err(|mut e: Fault| {
        e.unknown = mutation;
        e.message = format!(
            "local session request failed; inspect status before retrying: {}",
            e.message
        );
        e
    })
}
