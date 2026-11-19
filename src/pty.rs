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

/// Max shells (tabs) per host.
pub const MAX_SHELLS: usize = 8;

/// Output or exit of one shell, tagged so a stale message from a replaced shell can be ignored.
pub struct ShellMsg {
    pub id: u8,
    pub generation: u64,
    /// `None`: the shell exited.
    pub data: Option<Vec<u8>>,
}

/// A shell running in a pseudo-terminal.
pub struct Pty {
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    buf: Arc<Mutex<Buf>>,
    generation: u64,
}

impl Pty {
    /// Start a shell. Its output (while attached) and its exit go to `tx` tagged with `id`/`generation`.
    pub fn spawn(shell: &str, cwd: Option<&std::path::Path>, id: u8, generation: u64, tx: mpsc::Sender<ShellMsg>) -> Result<Self> {
        let pair = native_pty_system().openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })?;
        let child = pair.slave.spawn_command(shell_command(shell, cwd))?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

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
                if attached && tx.blocking_send(ShellMsg { id, generation, data: Some(chunk) }).is_err() {
                    return;
                }
            }
            let _ = tx.blocking_send(ShellMsg { id, generation, data: None });
        });
        Ok(Self { writer, master: pair.master, child, buf: shared, generation })
    }

    /// Still running (the shell has not exited)?
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Start live output; returns everything shown so far (to redraw the screen).
    fn attach(&mut self) -> Vec<u8> {
        let mut b = self.buf.lock().unwrap();
        b.attached = true;
        b.scroll.clone()
    }

    /// The phone went away: the shell keeps running, its output only goes to the scrollback.
    fn detach(&mut self) {
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

/// The host's set of shells (one per phone tab), shared by all phone connections: they keep
/// running while no phone is connected.
pub struct Shells {
    slots: Vec<Option<Pty>>,
    tx: mpsc::Sender<ShellMsg>,
    /// Output / exit messages of all attached shells.
    pub rx: mpsc::Receiver<ShellMsg>,
    next_generation: u64,
}

impl Default for Shells {
    fn default() -> Self {
        Self::new()
    }
}

impl Shells {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(256);
        Self { slots: (0..MAX_SHELLS).map(|_| None).collect(), tx, rx, next_generation: 1 }
    }

    pub fn get(&mut self, id: u8) -> Option<&mut Pty> {
        self.slots.get_mut(id as usize)?.as_mut()
    }

    /// Start a shell in slot `id` (a phone tab). Replaces nothing: fails if the slot is in use.
    /// The new shell is attached right away (a phone is connected when this is called).
    pub fn open(&mut self, id: u8, shell: &str, root: Option<&std::path::Path>) -> Result<()> {
        let n = id as usize;
        anyhow::ensure!(n < MAX_SHELLS, "at most {MAX_SHELLS} shells");
        anyhow::ensure!(self.slots[n].is_none(), "shell {id} already exists");
        let generation = self.next_generation;
        self.next_generation += 1;
        let mut p = Pty::spawn(shell, root, id, generation, self.tx.clone())?;
        p.attach();
        self.slots[n] = Some(p);
        Ok(())
    }

    /// Stop shell `id`.
    pub fn close(&mut self, id: u8) {
        if let Some(slot) = self.slots.get_mut(id as usize) {
            *slot = None; // Drop kills the child
        }
    }

    /// Handle an exit message: forget the shell. Returns true if it was the current one in its slot.
    pub fn on_exit(&mut self, msg: &ShellMsg) -> bool {
        match self.slots.get_mut(msg.id as usize) {
            Some(slot) if slot.as_ref().is_some_and(|p| p.generation == msg.generation) => {
                *slot = None;
                true
            }
            _ => false,
        }
    }

    /// Is this output for the shell currently in its slot (not a stale one)?
    pub fn is_current(&self, msg: &ShellMsg) -> bool {
        self.slots.get(msg.id as usize).and_then(|s| s.as_ref()).is_some_and(|p| p.generation == msg.generation)
    }

    /// A phone connected: drop shells that died while it was away, discard queued messages, start
    /// live output and return `(id, screen so far)` for every live shell.
    pub fn attach_all(&mut self) -> Vec<(u8, Vec<u8>)> {
        while self.rx.try_recv().is_ok() {}
        let mut out = Vec::new();
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if slot.as_mut().is_some_and(|p| !p.is_alive()) {
                *slot = None;
            }
            if let Some(p) = slot {
                out.push((i as u8, p.attach()));
            }
        }
        out
    }

    /// The phone went away: shells keep running, output only goes to their scrollback.
    pub fn detach_all(&mut self) {
        for p in self.slots.iter_mut().flatten() {
            p.detach();
        }
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
    use super::Shells;

    #[tokio::test]
    async fn spawn_in_pty_applies_root_and_guard() {
        let pwsh = r"C:\Program Files\PowerShell\7\pwsh.exe";
        let root = std::env::temp_dir().join("tt-guard-test").join("root");
        std::fs::create_dir_all(&root).unwrap();
        let mut sh = Shells::new();
        sh.open(0, pwsh, Some(&root)).unwrap();
        sh.get(0).unwrap().write(b"'LOC1=' + (Get-Location).Path; cd ..; 'LOC2=' + (Get-Location).Path; exit\r\n").unwrap();
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
