use std::{env, fs::OpenOptions, io::Write, path::PathBuf, thread};

fn main() {
    let arguments: Vec<_> = env::args_os().collect();
    if arguments.get(1).map(AsRef::as_ref) != Some(std::ffi::OsStr::new("--socket")) {
        std::process::exit(64);
    }
    let Some(socket_path) = arguments.get(2).map(PathBuf::from) else {
        std::process::exit(64);
    };
    if arguments.len() != 3 || !socket_path.is_absolute() {
        std::process::exit(64);
    }
    let Some(run_dir) = socket_path.parent() else {
        std::process::exit(64);
    };
    let mut pid_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(run_dir.join("fake-bridge.pid"))
        .expect("create fake Bridge pid");
    write!(pid_file, "{}", std::process::id()).expect("write fake Bridge pid");
    pid_file.sync_all().expect("sync fake Bridge pid");

    loop {
        thread::park();
    }
}
