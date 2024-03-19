//! Secure Element interface.

use close_fds::close_open_fds;
use data_encoding::BASE64;
use eyre::{bail, Result, WrapErr};
use std::{
    io::prelude::*,
    os::unix::process::CommandExt,
    process::{Command, Stdio},
};

/// Signs this buffer with Secure Element and returns the output.
pub fn sign<T: AsRef<[u8]>>(data: T) -> Result<Vec<u8>> {
    fn inner(data: &[u8]) -> Result<Vec<u8>> {
        let encoded = BASE64.encode(data);

        tracing::info!("Running orb-sign-iris-code");
        let mut command = Command::new("orb-sign-iris-code");
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        unsafe {
            command.pre_exec(|| {
                close_open_fds(libc::STDERR_FILENO + 1, &[]);
                Ok(())
            });
        }
        let mut child = command.spawn().wrap_err("running orb-sign-iris-code")?;

        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(encoded.as_bytes())?;
        drop(stdin);

        let output = child.wait_with_output().wrap_err("waiting for orb-sign-iris-code")?;
        let success = output.status.success();
        for line in String::from_utf8_lossy(&output.stderr).lines() {
            if success {
                tracing::trace!("orb-sign-iris-code {}", line);
            } else {
                tracing::error!("orb-sign-iris-code {}", line);
            }
        }
        if !success {
            if let Some(code) = output.status.code() {
                bail!("orb-sign-iris-code exited with non-zero exit code: {code}");
            } else {
                bail!("orb-sign-iris-code terminated by signal");
            }
        }
        BASE64.decode(&output.stdout).wrap_err("decoding orb-sign-iris-code output")
    }

    inner(data.as_ref())
}
