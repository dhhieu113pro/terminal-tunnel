use std::io::{Read, Write};

use anyhow::Result;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Output kept while no phone is attached, replayed when one (re)connects.
const SCROLLBACK: usize = 256 * 1024;

#[derive(Default)]
struct Buf {
    attached: bool,
    scroll: Vec<u8>,
}

/// PowerShell guard (works on Windows PowerShell 5.1 and 7): wrap the location cmdlets so a move
/// outside the root is undone. Functions beat cmdlets, but aliases beat functions, so the
/// aliases are removed first.
const GUARD_PS: &str = r#"
$global:ttroot = (Resolve-Path -LiteralPath '@ROOT@').ProviderPath.TrimEnd('\')
Set-Location -LiteralPath $global:ttroot
function global:tt-inside([string]$p) {
  $p = $p.TrimEnd('\')
  return ($p -eq $global:ttroot) -or $p.StartsWith($global:ttroot + '\', [StringComparison]::OrdinalIgnoreCase)
}
foreach ($a in 'cd','chdir','sl','pushd') { Remove-Item "alias:$a" -Force -ErrorAction SilentlyContinue }
function global:prompt {
  # The shared root is shown as "~" (like a home directory): PS ~> , PS ~\sub>
  $p = (Get-Location).ProviderPath
  if ($p.TrimEnd('\') -eq $global:ttroot) { $d = '~' }
  elseif ($p.StartsWith($global:ttroot + '\', [StringComparison]::OrdinalIgnoreCase)) { $d = '~' + $p.Substring($global:ttroot.Length) }
  else { $d = $p }
  "PS $d> "
}
function global:Set-Location {
  $old = (Get-Location).Path
  Microsoft.PowerShell.Management\Set-Location @args
  if (-not (tt-inside (Get-Location).ProviderPath)) {
    Write-Host "[terminal-tunnel] outside $global:ttroot is not allowed"
    Microsoft.PowerShell.Management\Set-Location -LiteralPath $old
  }
}
function global:Push-Location {
  $old = (Get-Location).Path
  Microsoft.PowerShell.Management\Push-Location @args
  if (-not (tt-inside (Get-Location).ProviderPath)) {
    Write-Host "[terminal-tunnel] outside $global:ttroot is not allowed"
    Microsoft.PowerShell.Management\Pop-Location
  }
}
Set-Alias cd Set-Location -Scope Global -Force -ErrorAction SilentlyContinue
Set-Alias sl Set-Location -Scope Global -Force -ErrorAction SilentlyContinue
Set-Alias chdir Set-Location -Scope Global -Force -ErrorAction SilentlyContinue
Set-Alias pushd Push-Location -Scope Global -Force -ErrorAction SilentlyContinue
"#;

/// Build the shell command. With a root it starts there; for PowerShell it also installs a
/// *soft* guard that undoes any `Set-Location`/`cd` leaving the root. This is not a security
/// boundary (full paths, other shells, `cmd`, etc. still work) - use OS permissions for that.
pub fn shell_command(shell: &str, root: Option<&std::path::Path>) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(shell);
    let Some(root) = root else { return cmd };
    cmd.cwd(root);
    let stem = std::path::Path::new(shell)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if stem == "powershell" || stem == "pwsh" {
        let r = root.to_string_lossy().trim_end_matches(['\\', '/']).replace('\'', "''");
        let script = GUARD_PS.replace("@ROOT@", &r);
        cmd.args(["-NoLogo", "-NoExit", "-Command", &script]);
    }
    cmd
}

/// A shell running in a pseudo-terminal. Output arrives on `out`.
pub struct Pty {
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    pub out: mpsc::Receiver<Vec<u8>>,
    buf: Arc<Mutex<Buf>>,
}

impl Pty {
    pub fn spawn(shell: &str, cwd: Option<&std::path::Path>) -> Result<Self> {
        let pair = native_pty_system().openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })?;
        let child = pair.slave.spawn_command(shell_command(shell, cwd))?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let (tx, out) = mpsc::channel::<Vec<u8>>(64);
        let shared = Arc::new(Mutex::new(Buf::default()));
        let thread_buf = shared.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let chunk = buf[..n].to_vec();
                let attached = {
                    let mut b = thread_buf.lock().unwrap();
                    b.scroll.extend_from_slice(&chunk);
                    if b.scroll.len() > SCROLLBACK {
                        let cut = b.scroll.len() - SCROLLBACK;
                        b.scroll.drain(..cut);
                    }
                    b.attached
                };
                if attached && tx.blocking_send(chunk).is_err() {
                    break;
                }
            }
        });
        Ok(Self { writer, master: pair.master, child, out, buf: shared })
    }

    /// Still running (the shell has not exited)?
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// A phone connects: returns everything shown so far (to redraw the screen) and starts live output.
    pub fn attach(&mut self) -> Vec<u8> {
        let mut b = self.buf.lock().unwrap();
        while self.out.try_recv().is_ok() {}
        b.attached = true;
        b.scroll.clone()
    }

    /// The phone went away: the shell keeps running, its output only goes to the scrollback.
    pub fn detach(&mut self) {
        self.buf.lock().unwrap().attached = false;
    }

    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
        Ok(())
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::shell_command;
    use std::path::Path;
    use std::process::Command;

    /// Runs the generated guard script, then `probe`, and returns the final location.
    fn run(shell: &str, root: &Path, probe: &str) -> String {
        let argv = shell_command(shell, Some(root)).get_argv().clone();
        // argv: [shell, -NoLogo, -NoExit, -Command, <script>]
        let script = format!("{}\n{probe}\n(Get-Location).Path", argv[4].to_string_lossy());
        let out = Command::new(shell)
            .args(["-NoLogo", "-NoProfile", "-Command", &script])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).lines().last().unwrap_or("").trim().to_string()
    }

    #[test]
    fn guard_blocks_escape_but_allows_inside() {
        let pwsh = r"C:\Program Files\PowerShell\7\pwsh.exe";
        let mut shells = vec!["powershell.exe"];
        if Path::new(pwsh).exists() {
            shells.push(pwsh);
        }
        for shell in shells {
            check(shell);
        }
    }

    fn check(shell: &str) {
        let run = |root: &Path, probe: &str| run(shell, root, probe);
        let base = std::env::temp_dir().join("tt-guard-test");
        let root = base.join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(base.join("root2")).unwrap(); // sibling sharing the name prefix
        let root_s = root.to_string_lossy().to_string();

        assert_eq!(run(&root, "Set-Location sub"), format!("{root_s}\\sub"));
        assert_eq!(run(&root, "Set-Location .."), root_s);
        assert_eq!(run(&root, "Set-Location ..\\root2"), root_s);
        assert_eq!(run(&root, "Set-Location C:\\"), root_s);
        // a blocked move reverts to where the shell was, not to the root
        assert_eq!(run(&root, "Set-Location sub; Set-Location ..\\.."), format!("{root_s}\\sub"));
        // aliases and pushd are covered too
        assert_eq!(run(&root, "cd .."), root_s);
        assert_eq!(run(&root, "sl .."), root_s);
        assert_eq!(run(&root, "chdir C:\\"), root_s);
        assert_eq!(run(&root, "pushd .."), root_s);
        assert_eq!(run(&root, "cd sub; cd .."), root_s);
        assert_eq!(run(&root, "cd -Path .."), root_s);
        assert_eq!(run(&root, "cd -LiteralPath C:\\Windows"), root_s);
        // a root written with forward slashes behaves the same
        let fwd = std::path::PathBuf::from(root_s.replace('\\', "/"));
        assert_eq!(run(&fwd, "cd sub"), format!("{root_s}\\sub"));
        assert_eq!(run(&fwd, "cd .."), root_s);
        assert_eq!(run(&fwd, "cd C:\\"), root_s);

        // the prompt shows the root as "~" like a home directory
        let prompt_line = |probe: &str| {
            let argv = shell_command(shell, Some(&root)).get_argv().clone();
            let script = format!("{}\n{probe}\n'P=[' + (prompt) + ']'", argv[4].to_string_lossy());
            let out = Command::new(shell).args(["-NoLogo", "-NoProfile", "-Command", &script]).output().unwrap();
            String::from_utf8_lossy(&out.stdout).lines().last().unwrap_or("").trim().to_string()
        };
        assert_eq!(prompt_line(""), "P=[PS ~> ]");
        assert_eq!(prompt_line("cd sub"), "P=[PS ~\\sub> ]");
        assert_eq!(prompt_line("cd sub; cd .."), "P=[PS ~> ]");
    }
}

#[cfg(all(test, windows))]
mod pty_tests {
    use super::Pty;

    #[tokio::test]
    async fn spawn_in_pty_applies_root_and_guard() {
        let pwsh = r"C:\Program Files\PowerShell\7\pwsh.exe";
        let root = std::env::temp_dir().join("tt-guard-test").join("root");
        std::fs::create_dir_all(&root).unwrap();
        let mut p = Pty::spawn(pwsh, Some(&root)).unwrap();
        p.attach();
        p.write(b"'LOC1=' + (Get-Location).Path; cd ..; 'LOC2=' + (Get-Location).Path; exit\r\n").unwrap();
        let mut all = String::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            while let Some(b) = p.out.recv().await {
                let s = String::from_utf8_lossy(&b).to_string();
                if s.contains("\x1b[6n") {
                    p.write(b"\x1b[1;1R").unwrap(); // answer the cursor-position query like a terminal
                }
                all.push_str(&s);
            }
        })
        .await;
        println!("PTY OUTPUT:\n{all}");
        assert!(all.contains(&format!("LOC1={}", root.display())), "start dir not applied");
        assert!(all.contains(&format!("LOC2={}", root.display())), "guard not applied");
        assert!(all.contains("PS ~>"), "prompt should show the root as ~");
    }
}
