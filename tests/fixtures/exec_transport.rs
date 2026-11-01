use std::io::{self, Write};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let script = args.last().unwrap().as_str();
    if script == "hold" {
        std::thread::sleep(Duration::from_secs(2));
        return;
    }
    assert!(args.contains(&"-T".into()));
    assert!(args.contains(&"BatchMode=yes".into()));
    assert!(args.contains(&"StrictHostKeyChecking=yes".into()));
    assert!(args.contains(&"--".into()));
    match script {
        "outputs" => {
            print!("  stdout 雪\n\n{} ", "long_filename_".repeat(40));
            eprint!("stderr without newline");
        }
        "delayed" | "silent" => {
            if script == "delayed" {
                print!("first\n");
                io::stdout().flush().unwrap();
            }
            std::thread::sleep(Duration::from_millis(600));
            print!("finished\n");
        }
        "nonzero" => {
            print!("partial stdout\n");
            eprint!("command failed\n");
            std::process::exit(42);
        }
        "transport" => {
            eprint!("connection lost\n");
            std::process::exit(255);
        }
        "timeout" => {
            print!("before timeout\n");
            eprint!("diagnostic\n");
            io::stdout().flush().unwrap();
            io::stderr().flush().unwrap();
            std::thread::sleep(Duration::from_secs(5));
        }
        "inherited" => {
            print!("parent output\n");
            io::stdout().flush().unwrap();
            let _ = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("hold")
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .spawn().unwrap();
        }
        "large" => {
            let out = std::thread::spawn(|| io::stdout().write_all(&vec![b'x'; 1_100_000]).unwrap());
            io::stderr().write_all(&vec![b'y'; 1_100_000]).unwrap();
            out.join().unwrap();
        }
        "args" => print!("{}", args.join("|")),
        _ => print!("{script}"),
    }
}
