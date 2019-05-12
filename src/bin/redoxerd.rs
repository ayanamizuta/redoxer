extern crate redox_termios;
extern crate syscall;

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::io::{FromRawFd, IntoRawFd, RawFd};
use std::process::{Child, Command, ExitStatus, Stdio};
use syscall::{Io, Pio};

const DEFAULT_COLS: u32 = 80;
const DEFAULT_LINES: u32 = 30;

fn syscall_error(error: syscall::Error) -> io::Error {
    io::Error::from_raw_os_error(error.errno)
}

pub fn handle(event_file: &mut File, master_fd: RawFd, process: &mut Child) -> io::Result<ExitStatus> {
    let handle_event = |event_id: RawFd| -> io::Result<bool> {
        if event_id == master_fd {
            let mut packet = [0; 4096];
            loop {
                let count = match syscall::read(master_fd as usize, &mut packet) {
                    Ok(0) => return Ok(false),
                    Ok(count) => count,
                    Err(ref err) if err.errno == syscall::EAGAIN => return Ok(true),
                    Err(err) => return Err(syscall_error(err)),
                };
                for i in 1..count {
                    // Write byte to QEMU debugcon (Bochs compatible)
                    Pio::<u8>::new(0xe9).write(packet[i]);
                }
            }
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected event id {}", event_id)
            ))
        }
    };

    if handle_event(master_fd)? {
        'events: loop {
            let mut sys_event = syscall::Event::default();
            event_file.read(&mut sys_event)?;
            if ! handle_event(sys_event.id as RawFd)? {
                break 'events;
            }

            match process.try_wait() {
                Ok(status_opt) => match status_opt {
                    Some(status) => return Ok(status),
                    None => ()
                },
                Err(err) => match err.kind() {
                    io::ErrorKind::WouldBlock => (),
                    _ => return Err(err),
                }
            }
        }
    }

    let _ = process.kill();
    process.wait()
}

pub fn getpty(columns: u32, lines: u32) -> io::Result<(RawFd, String)> {
    let master = syscall::open("pty:", syscall::O_CLOEXEC | syscall::O_RDWR | syscall::O_CREAT | syscall::O_NONBLOCK)
        .map_err(syscall_error)?;

    if let Ok(winsize_fd) = syscall::dup(master, b"winsize") {
        let _ = syscall::write(winsize_fd, &redox_termios::Winsize {
            ws_row: lines as u16,
            ws_col: columns as u16
        });
        let _ = syscall::close(winsize_fd);
    }

    let mut buf: [u8; 4096] = [0; 4096];
    let count = syscall::fpath(master, &mut buf).map_err(syscall_error)?;
    Ok((master as RawFd, unsafe { String::from_utf8_unchecked(Vec::from(&buf[..count])) }))
}

fn inner() -> io::Result<()> {
    unsafe { syscall::iopl(3).map_err(syscall_error)?; }

    let config = fs::read_to_string("/etc/redoxerd")?;
    let mut config_lines = config.lines();

    let (columns, lines) = (DEFAULT_COLS, DEFAULT_LINES);
    let (master_fd, pty) = getpty(columns, lines)?;

    let mut event_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open("event:")?;

    event_file.write(&syscall::Event {
        id: master_fd as usize,
        flags: syscall::flag::EVENT_READ,
        data: 0
    })?;

    let slave_stdin = OpenOptions::new().read(true).open(&pty)?;
    let slave_stdout = OpenOptions::new().write(true).open(&pty)?;
    let slave_stderr = OpenOptions::new().write(true).open(&pty)?;

    if let Some(name) = config_lines.next() {
        let mut command = Command::new(name);
        for arg in config_lines {
            command.arg(arg);
        }
        unsafe {
            command
            .stdin(Stdio::from_raw_fd(slave_stdin.into_raw_fd()))
            .stdout(Stdio::from_raw_fd(slave_stdout.into_raw_fd()))
            .stderr(Stdio::from_raw_fd(slave_stderr.into_raw_fd()))
            .env("COLUMNS", format!("{}", columns))
            .env("LINES", format!("{}", lines))
            .env("TERM", "xterm-256color")
            .env("TTY", &pty);
        }

        let mut process = command.spawn()?;
        let status = handle(&mut event_file, master_fd, &mut process)?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                format!("{}", status)
            ))
        }
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "/etc/redoxerd does not specify command"
        ))
    }
}

pub fn main() {
    match inner() {
        Ok(()) => {
            // Exit with success using qemu device
            Pio::<u16>::new(0x604).write(0x2000);
            Pio::<u8>::new(0x501).write(51 / 2);
        },
        Err(err) => {
            eprintln!("redoxerd: {}", err);
            // Exit with error using qemu device
            Pio::<u8>::new(0x501).write(53 / 2);
        }
    }
}
