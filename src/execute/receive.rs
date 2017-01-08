use std::fs::{self, File};
use std::io::{self, Write, Read};
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::Duration;

use disk_buffer::DiskBuffer;
use filepaths;
use arguments::Args;
use super::pipe::disk::State;
use smallvec::SmallVec;

/// Reads the standard output and error files of the current unit, writing them to the standard output/error.
macro_rules! read_outputs {
    ($stdout:ident, $stderr:ident, $buffer:ident, $stdout_out:ident, $stderr_out:ident) => {
        let mut bytes_read = $stdout.read(&mut $buffer).unwrap_or(0);
        while bytes_read != 0 {
            if let Err(why) = $stdout_out.write(&$buffer[0..bytes_read]) {
                let _ = write!($stderr_out, "parallel: I/O error: unable to write to standard output: {}\n", why);
            }
            bytes_read = $stdout.read(&mut $buffer).unwrap_or(0);
        }

        bytes_read = $stderr.read(&mut $buffer).unwrap_or(0);
        while bytes_read != 0 {
            if let Err(why) = $stderr_out.write(&$buffer[0..bytes_read]) {
                let _ = write!($stderr_out, "parallel: I/O error: unable to write to standard error: {}\n", why);
            }
            bytes_read = $stderr.read(&mut $buffer).unwrap_or(0);
        }
    }
}

/// Removes both the standard output and error file of the current job
macro_rules! remove_job_files {
    ($stdout_path:ident, $stderr_path:ident, $stderr:ident) => {{
        if let Err(why) = fs::remove_file(&$stdout_path).and_then(|_| fs::remove_file(&$stderr_path)) {
            let _ = write!($stderr, "parallel: I/O error: unable to remove job files: {}\n", why);
        }
    }}
}

/// Opens the standard output and error files of the next job, attempting repeatedly until a success.
macro_rules! open_job_files {
    ($stdout_path:ident, $stderr_path:ident) => {{
        let stdout_file = loop {
            if let Ok(file) = File::open(&$stdout_path) { break file }
            thread::sleep(Duration::from_millis(1));
        };

        let stderr_file = loop {
            if let Ok(file) = File::open(&$stderr_path) { break file }
            thread::sleep(Duration::from_millis(1));
        };

        (stdout_file, stderr_file)
    }}
}

/// Append the current job to the processed file
macro_rules! append_to_processed {
    ($processed:ident, $input:ident, $stderr:ident) => {{
        if let Err(why) = $processed.write($input.as_bytes()).and_then(|_| $processed.write(b"\n")) {
            let _ = write!($stderr, "parallel: I/O error: unable to append to processed: {}\n", why);
        }
    }}
}

#[allow(cyclomatic_complexity)]
/// Tail and print the standard output and error of each process in the correct order
pub fn receive_messages(input_rx: Receiver<State>, args: Args, processed_path: &Path, errors_path: &Path) {
    let stdout = io::stdout();
    let stderr = io::stderr();

    // Keeps track of which job is currently allowed to print to standard output/error.
    let mut counter = 0;
    // The following `buffer` is used to store completed jobs that are awaiting processing.
    let mut buffer = SmallVec::<[State; 32]>::new();
    // Store a list of indexes that we need to drop from `buffer` after a match has been found.
    let mut drop = SmallVec::<[usize; 32]>::new();
    // An opened disk buffer pointing to the processed file.
    let mut processed_file = DiskBuffer::new(processed_path).write().unwrap();
    // An opened disk buffer pointing to the error file.
    let mut error_file     = DiskBuffer::new(errors_path).write().unwrap();
    // A buffer for buffering the outputs of temporary files on disk.
    let mut read_buffer = [0u8; 8192];
    // A buffer for converting job ID's into a byte array representation of a string.
    let mut id_buffer = [0u8; 64];
    // Generates the stdout and stderr paths, along with a truncation value to truncate the job ID from the paths.
    let (truncate_size, mut stdout_path, mut stderr_path) = filepaths::new_job(counter);

    // The loop will only quit once all inputs have been processed
    while counter < args.ninputs {
        let mut tail_next = false;

        match input_rx.recv().unwrap() {
            State::Completed(id, name) => {
                if id == counter {
                    let mut stdout = stdout.lock();
                    let mut stderr = stderr.lock();
                    filepaths::next_job_path(counter, truncate_size, &mut id_buffer, &mut stdout_path, &mut stderr_path);
                    let (mut stdout_file, mut stderr_file) = open_job_files!(stdout_path, stderr_path);
                    append_to_processed!(processed_file, name, stderr);
                    read_outputs!(stdout_file, stderr_file, read_buffer, stdout, stderr);
                    remove_job_files!(stdout_path, stderr_path, stderr);
                    counter += 1;
                } else {
                    buffer.push(State::Completed(id, name));
                    tail_next = true;
                }
            },
            State::Error(id, message) => {
                if id == counter {
                    counter += 1;
                    if let Err(why) = error_file.write(message.as_bytes()) {
                        let mut stderr = stderr.lock();
                        let _ = write!(stderr, "parallel: I/O error: {}", why);
                    }
                } else {
                    buffer.push(State::Error(id, message));
                }
            }
        }

        if tail_next {
            filepaths::next_job_path(counter, truncate_size, &mut id_buffer, &mut stdout_path, &mut stderr_path);
            let (mut stdout_file, mut stderr_file) = open_job_files!(stdout_path, stderr_path);

            loop {
                match input_rx.try_recv() {
                    Ok(State::Completed(id, name)) => {
                        if id == counter {
                            let mut stdout = stdout.lock();
                            let mut stderr = stderr.lock();
                            append_to_processed!(processed_file, name, stderr);
                            read_outputs!(stdout_file, stderr_file, read_buffer, stdout, stderr);
                            remove_job_files!(stdout_path, stderr_path, stderr);
                            counter += 1;
                            break
                        } else {
                            buffer.push(State::Completed(id, name));
                        }
                    },
                    Ok(State::Error(id, message)) => {
                        if id == counter {
                            counter += 1;
                            if let Err(why) = error_file.write(message.as_bytes()) {
                                let mut stderr = stderr.lock();
                                let _ = write!(stderr, "parallel: I/O error: {}", why);
                            }
                        } else {
                            buffer.push(State::Error(id, message));
                        }
                    },
                    _ => {
                        let mut stdout = stdout.lock();
                        let mut stderr = stderr.lock();
                        let mut bytes_read = stdout_file.read(&mut read_buffer).unwrap();
                        if bytes_read != 0 { stdout.write(&read_buffer[0..bytes_read]).unwrap(); }

                        bytes_read = stderr_file.read(&mut read_buffer).unwrap();
                        if bytes_read != 0 { stderr.write(&read_buffer[0..bytes_read]).unwrap(); }
                        thread::sleep(Duration::from_millis(1));
                    }
                }
            }
        }

        let mut changed = true;
        while changed {
            changed = false;
            for (index, state) in buffer.iter().enumerate() {
                match *state {
                    State::Completed(id, ref name) if id == counter => {
                        let mut stdout = stdout.lock();
                        let mut stderr = stderr.lock();
                        filepaths::next_job_path(counter, truncate_size, &mut id_buffer, &mut stdout_path, &mut stderr_path);
                        let (mut stdout_file, mut stderr_file) = open_job_files!(stdout_path, stderr_path);
                        append_to_processed!(processed_file, name, stderr);
                        read_outputs!(stdout_file, stderr_file, read_buffer, stdout, stderr);
                        remove_job_files!(stdout_path, stderr_path, stderr);
                        counter += 1;
                        changed = true;
                        drop.push(index);
                    },
                    State::Error(id, ref message) if id == counter => {
                        counter += 1;
                        if let Err(why) = error_file.write(message.as_bytes()) {
                            let mut stderr = stderr.lock();
                            let _ = write!(stderr, "parallel: I/O error: {}", why);
                        }
                    },
                    _ => ()
                }
            }
        }

        drop_used_values(&mut buffer, &mut drop);
    }

    if let Err(why) = processed_file.flush() {
        let mut stderr = stderr.lock();
        let _ = write!(stderr, "parallel: I/O error: {}", why);
    }

    if let Err(why) = error_file.flush() {
        let mut stderr = stderr.lock();
        let _ = write!(stderr, "parallel: I/O error: {}", why);
    }
}

fn drop_used_values(buffer: &mut SmallVec<[State; 32]>, drop: &mut SmallVec<[usize; 32]>) {
    drop.sort();
    for id in drop.drain().rev() {
        let _ = buffer.remove(id);
    }
}