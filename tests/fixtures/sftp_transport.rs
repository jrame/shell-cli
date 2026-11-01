// Standalone fake SSH executable used by the transport deadline regression test.
use std::io::{Read, Write};

fn receive() -> Vec<u8> {
    let mut length = [0; 4];
    std::io::stdin().read_exact(&mut length).unwrap();
    let mut data = vec![0; u32::from_be_bytes(length) as usize];
    std::io::stdin().read_exact(&mut data).unwrap();
    data
}
fn send(kind: u8, number: u32, body: &[u8]) {
    let mut out = std::io::stdout().lock();
    out.write_all(&((body.len() + 5) as u32).to_be_bytes()).unwrap();
    out.write_all(&[kind]).unwrap();
    out.write_all(&number.to_be_bytes()).unwrap();
    out.write_all(body).unwrap();
    out.flush().unwrap();
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    for expected in ["BatchMode=yes", "StrictHostKeyChecking=yes", "ClearAllForwardings=yes", "ControlPath=none", "-s"] {
        assert!(args.iter().any(|arg| arg == expected));
    }
    assert!(!args.iter().any(|arg| matches!(arg.as_str(), "-L" | "-R" | "-D")));
    assert_eq!(args.last().unwrap(), "sftp");
    let mode = &args[args.len() - 2];
    if mode == "denied" {
        eprintln!("Permission denied (publickey).");
        std::process::exit(255);
    }
    if mode == "after-open" {
        assert_eq!(receive()[0], 1); // INIT
        send(2, 3, b""); // VERSION
        let request = receive();
        assert_eq!(request[0], 7); // LSTAT
        let id = u32::from_be_bytes(request[1..5].try_into().unwrap());
        send(101, id, &[0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0]); // NO_SUCH_FILE
        let request = receive();
        assert_eq!(request[0], 3); // OPEN, whose response is deliberately lost.
    }
    eprintln!("mock SSH stalled");
    std::thread::sleep(std::time::Duration::from_secs(10));
}
