use std::{
    io::{self, Write},
    path::Path,
};

pub fn server_init<P: AsRef<Path>>(_socket_path: P) -> Result<(), std::io::Error> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "server mode is not supported on windows",
    ))
}

pub struct Client {}

impl Client {
    pub fn new<P: AsRef<Path>>(_socket_path: P) -> Result<Client, io::Error> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "client IPC is not supported on windows",
        ))
    }

    pub fn block_read(&mut self) {}

    pub fn send_str(&mut self, _data: &str) {}
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
    Box::new(std::io::stdout()) as Box<dyn Write>
}

pub fn setup_client_stdin_out() -> Result<(), ()> {
    Ok(())
}
