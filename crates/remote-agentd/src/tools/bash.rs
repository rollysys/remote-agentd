//! remote_bash tool — execute a shell command and return combined output.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// Tool implementing shell command execution.
pub struct BashTool;

impl BashTool {
    /// Execute the bash tool.
    ///
    /// Input: `{ command: string, cwd?: string, timeout?: number, env?: object }`
    /// Runs `sh -c "<command>"`, merges stdout+stderr, returns output + exit code.
    /// Sets `isError: true` when the process exits non-zero.
    pub fn execute(args: &Value) -> Result<Value> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("`command` is required and must be a string"))?;

        let sudo = args.get("sudo").and_then(|v| v.as_bool()).unwrap_or(false);
        let cwd = args.get("cwd").and_then(|v| v.as_str());
        let timeout_secs = args
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(60);
        let env_obj = args.get("env").and_then(|v| v.as_object());

        // When sudo is requested, wrap the command in `sudo -n sh -c "..."`.
        // `-n` ensures non-interactive mode (fails fast if NOPASSWD is not
        // configured, instead of hanging on a password prompt that would
        // deadlock the MCP stdio loop).
        let mut cmd = if sudo {
            let mut c = Command::new("sudo");
            c.arg("-n").arg("sh").arg("-c").arg(command);
            c
        } else {
            let mut c = Command::new("sh");
            c.arg("-c").arg(command);
            c
        };
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        if let Some(env_map) = env_obj {
            for (key, val) in env_map {
                if let Some(val_str) = val.as_str() {
                    cmd.env(key, val_str);
                }
            }
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow!("Failed to spawn command: {}", e))?;

        // Drain stdout and stderr on background threads so the pipes don't
        // fill and deadlock the child.
        let (tx, rx) = mpsc::channel();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let tx_out = tx.clone();
        let stdout_handle = std::thread::spawn(move || {
            if let Some(mut s) = stdout {
                let mut buf = Vec::new();
                s.read_to_end(&mut buf).ok();
                let _ = tx_out.send(("stdout", buf));
            }
        });
        let tx_err = tx;
        let stderr_handle = std::thread::spawn(move || {
            if let Some(mut s) = stderr {
                let mut buf = Vec::new();
                s.read_to_end(&mut buf).ok();
                let _ = tx_err.send(("stderr", buf));
            }
        });

        // Poll for completion with a timeout (std has no wait_timeout).
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if Instant::now() > deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = stdout_handle.join();
                        let _ = stderr_handle.join();
                        let mut combined = String::new();
                        while let Ok((_, buf)) = rx.try_recv() {
                            if !buf.is_empty() {
                                if !combined.is_empty() {
                                    combined.push('\n');
                                }
                                combined.push_str(&String::from_utf8_lossy(&buf));
                            }
                        }
                        let mut msg = format!("Command timed out after {}s", timeout_secs);
                        if !combined.is_empty() {
                            msg.push_str("\n\n");
                            msg.push_str(&combined);
                        }
                        return Ok(json!({
                            "isError": true,
                            "content": [{ "type": "text", "text": msg }]
                        }));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => return Err(anyhow!("Failed to wait for command: {}", e)),
            }
        };

        let _ = stdout_handle.join();
        let _ = stderr_handle.join();

        // Collect output in stdout-then-stderr order.
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();
        while let Ok((which, buf)) = rx.try_recv() {
            match which {
                "stdout" => stdout_buf = buf,
                "stderr" => stderr_buf = buf,
                _ => {}
            }
        }

        let mut combined = String::from_utf8_lossy(&stdout_buf).into_owned();
        if !stderr_buf.is_empty() {
            let s = String::from_utf8_lossy(&stderr_buf);
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&s);
        }

        let code = status.code().unwrap_or(-1);
        let is_error = !status.success();

        let mut result = json!({
            "content": [{ "type": "text", "text": combined }],
            "exitCode": code,
        });
        if is_error {
            result["isError"] = json!(true);
        }
        Ok(result)
    }
}
