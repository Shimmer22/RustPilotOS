use polling::{Event, Events, Poller};
use sendfd::{RecvWithFd, SendWithFd};
use std::{
    collections::HashMap,
    io::{self, BufRead, BufReader, Read, Write},
    mem::MaybeUninit,
    os::{
        fd::AsRawFd,
        unix::net::{UnixListener, UnixStream},
    },
    path::Path,
    sync::LazyLock,
};

use crate::{module::Module, pthread_scheduler::SchedulePthread};

fn debug_enabled() -> bool {
    std::env::var("LINTX_DEBUG")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "on" | "ON"))
        .unwrap_or(false)
}

fn debug_log(msg: &str) {
    if debug_enabled() {
        eprintln!("[lintx-debug][server] {msg}");
    }
}

#[repr(C)]
struct ThreadSpecificData {
    stream: *mut UnixStream,
    client_stdin: libc::c_int,
    client_stdout: libc::c_int,
}

static PTHREAD_KEY: LazyLock<libc::pthread_key_t> = LazyLock::new(|| {
    let mut key: MaybeUninit<libc::pthread_key_t> = MaybeUninit::zeroed();
    unsafe {
        libc::pthread_key_create(key.as_mut_ptr(), Some(drop_specifidata));
        key.assume_init()
    }
});

fn set_thread_specifidata(data: ThreadSpecificData) {
    let data = Box::new(data);
    let data = Box::leak(data);
    unsafe {
        libc::pthread_setspecific(
            *PTHREAD_KEY,
            &*data as *const ThreadSpecificData as *const libc::c_void,
        );
    }
}

fn get_thread_specifidata() -> Option<&'static ThreadSpecificData> {
    unsafe {
        let ret = libc::pthread_getspecific(*PTHREAD_KEY) as *const ThreadSpecificData;
        if ret == std::ptr::null() {
            return None;
        }
        Some(&*ret)
    }
}

unsafe extern "C" fn drop_specifidata(ptr: *mut libc::c_void) {
    unsafe {
        drop(Box::from_raw(ptr as *mut ThreadSpecificData));
    };
}

pub fn server_init<P: AsRef<Path>>(socket_path: P) -> Result<(), std::io::Error> {
    let socket_path = socket_path.as_ref();
    debug_log(&format!(
        "server_init socket_path={} cwd={}",
        socket_path.display(),
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<unknown>".to_string())
    ));

    match std::fs::remove_file(socket_path) {
        Ok(()) => debug_log(&format!("removed stale socket {}", socket_path.display())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(io::Error::new(
                err.kind(),
                format!(
                    "failed to remove stale socket `{}`: {}",
                    socket_path.display(),
                    err
                ),
            ));
        }
    }

    let listener = UnixListener::bind(socket_path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "failed to bind unix socket `{}`: {}",
                socket_path.display(),
                err
            ),
        )
    })?;
    listener.set_nonblocking(true).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "failed to set nonblocking on unix socket `{}`: {}",
                socket_path.display(),
                err
            ),
        )
    })?;

    let poller = Poller::new().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("failed to create poller for server socket: {}", err),
        )
    })?;
    let mut fd_thread_map: HashMap<usize, libc::pthread_t> = HashMap::new();
    loop {
        match listener.accept() {
            Ok((mut client, _)) => {
                debug_log(&format!("accepted fd={}", client.as_raw_fd()));
                let mut fds: [libc::c_int; 2] = [0; 2];
                let mut buf: [u8; 10] = [0; 10];
                let recv_ret = unsafe {
                    client.recv_with_fd(
                        &mut buf,
                        std::slice::from_raw_parts_mut(fds.as_mut_ptr(), 2),
                    )
                };
                let (recv_n, _) = match recv_ret {
                    Ok(ret) => ret,
                    Err(err) => {
                        debug_log(&format!("recv_with_fd failed: {}", err));
                        continue;
                    }
                };
                if recv_n == 0 {
                    debug_log("recv_with_fd returned 0 bytes");
                    continue;
                }

                let mut cmd_bytes = Vec::with_capacity(512);
                // First byte is handshake marker (`15`) from client.
                // If command bytes are coalesced in same packet, preserve them.
                if recv_n > 1 {
                    cmd_bytes.extend_from_slice(&buf[1..recv_n]);
                }

                // Keep this large enough for typical module invocations with multiple flags
                // (e.g. ui_demo + touch path + resolution + fps).
                let mut buffer = [0; 4096];
                let read_n = match client.read(&mut buffer) {
                    Ok(n) => n,
                    Err(err) => {
                        debug_log(&format!("read command failed: {}", err));
                        continue;
                    }
                };
                if read_n == 0 && cmd_bytes.is_empty() {
                    debug_log("empty command from client");
                    continue;
                }
                if read_n > 0 {
                    cmd_bytes.extend_from_slice(&buffer[..read_n]);
                }

                let cmd_raw = String::from_utf8_lossy(&cmd_bytes)
                    .trim_matches(char::from(0))
                    .trim()
                    .to_string();
                debug_log(&format!("raw command=`{}`", cmd_raw));

                let mut args: Vec<String> =
                    cmd_raw.split_whitespace().map(|x| x.to_string()).collect();
                if args.is_empty() {
                    continue;
                }

                let detached = if args.first().unwrap() == "__DETACH__" {
                    args.remove(0);
                    true
                } else {
                    false
                };

                if args.is_empty() {
                    continue;
                }

                if args[0] == "shutdown" {
                    break;
                }

                #[cfg(target_os = "macos")]
                if args.first().map(|x| x.as_str()) == Some("ui_demo") {
                    let argv_owned = args;
                    let argv: Vec<&str> = argv_owned.iter().map(|x| x.as_str()).collect();
                    if debug_enabled() {
                        let joined = argv.join(" ");
                        eprintln!("[lintx-debug][server-main] detached={detached} argv={joined}");
                    }
                    if !detached {
                        let data = ThreadSpecificData {
                            stream: &mut client as *mut UnixStream,
                            client_stdin: fds[0],
                            client_stdout: fds[1],
                        };
                        set_thread_specifidata(data);
                    }
                    if let Some(module) = Module::try_get_module(argv[0]) {
                        module.execute((argv.len()) as u32, argv.as_ptr());
                    } else if !detached {
                        let _ = writeln!(client, "[lintx] unknown module: {}", argv[0]);
                    } else {
                        eprintln!("[lintx] unknown module: {}", argv[0]);
                    }
                    if !detached {
                        let _ = client.shutdown(std::net::Shutdown::Both);
                    }
                    continue;
                }

                let mut client_cp = match client.try_clone() {
                    Ok(cp) => cp,
                    Err(err) => {
                        debug_log(&format!("try_clone failed: {}", err));
                        continue;
                    }
                };
                let argv_owned = args;

                let x = SchedulePthread::new_simple(Box::new(move |_| {
                    let argv: Vec<&str> = argv_owned.iter().map(|x| x.as_str()).collect();
                    if debug_enabled() {
                        let joined = argv.join(" ");
                        eprintln!("[lintx-debug][server-worker] detached={detached} argv={joined}");
                    }
                    if !detached {
                        let data = ThreadSpecificData {
                            stream: &mut client_cp as *mut UnixStream,
                            client_stdin: fds[0],
                            client_stdout: fds[1],
                        };
                        set_thread_specifidata(data);
                    }
                    if let Some(module) = Module::try_get_module(argv[0]) {
                        module.execute((argv.len()) as u32, argv.as_ptr());
                    } else if !detached {
                        let _ = writeln!(client_cp, "[lintx] unknown module: {}", argv[0]);
                    } else {
                        eprintln!("[lintx] unknown module: {}", argv[0]);
                    }
                    if !detached {
                        _ = client_cp.shutdown(std::net::Shutdown::Both);
                    }
                }));

                if !detached {
                    if let Err(err) = unsafe {
                        poller.add(
                            &client,
                            Event::none(client.as_raw_fd() as usize).with_interrupt(),
                        )
                    } {
                        debug_log(&format!("poller.add failed: {}", err));
                        continue;
                    }
                    fd_thread_map.insert(client.as_raw_fd() as usize, x.thread_id);
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
            Err(err) => {
                debug_log(&format!("accept failed: {}", err));
            }
        }

        let mut events = Events::new();
        let _ = poller.wait(&mut events, Some(std::time::Duration::from_secs(1)));

        for ev in events.iter() {
            if let Some(thread) = fd_thread_map.remove(&ev.key) {
                unsafe {
                    libc::pthread_cancel(thread); // some memory may leak
                }
            } else if debug_enabled() {
                eprintln!("[lintx-debug][server] got event for unknown key={}", ev.key);
            }
        }
    }

    Ok(())
}

pub struct Client {
    stream: UnixStream,
}

impl Client {
    pub fn new<P: AsRef<Path>>(socket_path: P) -> Result<Client, io::Error> {
        let stream = UnixStream::connect(socket_path.as_ref())?;
        let mut client = Client { stream };
        client.send_stdin_out();
        Ok(client)
    }

    pub fn block_read(&mut self) {
        let mut bufreader = BufReader::new(self.stream.try_clone().unwrap());
        let mut str_out: String = String::new();
        while let Ok(n) = bufreader.read_line(&mut str_out) {
            if n == 0 {
                break;
            }
            print!("{}", str_out);
            str_out.clear();
        }
    }

    pub fn send_str(&mut self, data: &str) {
        self.stream.write_all(data.as_bytes()).unwrap();
        self.stream.flush().unwrap();
    }

    fn send_stdin_out(&mut self) {
        let pipe: [libc::c_int; 2] = [std::io::stdin().as_raw_fd(), std::io::stdout().as_raw_fd()];
        unsafe {
            self.stream
                .send_with_fd(&[15], std::slice::from_raw_parts(pipe.as_ptr(), 2))
                .unwrap();
        }
    }
}

#[macro_export]
macro_rules! thread_logln {
    ($($arg:tt)*) => {
        write!(rpos::server_client::get_output(),"{}\n", format!($($arg)*)).unwrap()
    }
}

#[macro_export]
macro_rules! thread_log {
    ($($arg:tt)*) => {
        write!(rpos::server_client::get_output(),"{}", format!($($arg)*)).unwrap()
    }
}

pub fn get_output() -> Box<dyn Write> {
    let thread_data = unsafe { libc::pthread_getspecific(*PTHREAD_KEY) };
    if thread_data == std::ptr::null_mut() {
        Box::new(std::io::stdout()) as Box<dyn Write>
    } else {
        let stream: &ThreadSpecificData = unsafe { &mut *(thread_data as *mut ThreadSpecificData) };
        unsafe { Box::new((*stream.stream).try_clone().unwrap()) as Box<dyn Write> }
    }
}

pub fn setup_client_stdin_out() -> Result<(), ()> {
    unsafe {
        let sp = get_thread_specifidata();

        if sp.is_none() {
            return Err(());
        }
        let sp = sp.unwrap();
        let mut s: MaybeUninit<libc::termios> = MaybeUninit::zeroed();
        libc::tcgetattr(sp.client_stdin, s.as_mut_ptr());
        let mut term = s.assume_init();
        term.c_lflag &= !libc::ICANON;
        term.c_lflag &= !libc::ECHO;
        libc::tcsetattr(
            sp.client_stdin,
            libc::TCSANOW,
            &term as *const libc::termios,
        );

        libc::dup2(sp.client_stdin, libc::STDIN_FILENO);
        libc::dup2(sp.client_stdout, libc::STDOUT_FILENO);
    }
    Ok(())
}
