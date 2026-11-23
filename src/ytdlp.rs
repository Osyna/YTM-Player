//! The player's own copy of yt-dlp, kept current.
//!
//! YouTube changes what it serves every few weeks and yt-dlp follows within days; a
//! distribution's package follows within months, and a player that resolved everything
//! the day it was installed quietly stops resolving anything. So the first time a session
//! needs yt-dlp - at most once a day - this asks GitHub which release is the latest, and
//! when neither the system's copy nor ours is that recent it downloads the release into
//! the state directory and runs that instead. With no yt-dlp installed at all, the same
//! path is what installs one.
//!
//! Every way this can fail - no network, no `curl`, a download that is not the file the
//! release lists, a release file that will not start here - leaves things as they were:
//! whatever yt-dlp already ran keeps running. The download is `curl`, and its checksum
//! `sha256sum`, for the reason Spotify pages are `curl`: this player drives programs
//! rather than linking a TLS stack or a hash.

use crate::{store, youtube};
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

const RELEASES: &str = "https://github.com/yt-dlp/yt-dlp/releases";
/// How long an answer to "which release is the latest" is trusted.
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// What a check did, waiting to be shown. The interface owns the screen, so nothing here
/// prints: the event loop collects this and puts it up as a toast.
static NOTICE: Mutex<Option<String>> = Mutex::new(None);

/// The latest thing a background check has to say, once.
pub fn take_notice() -> Option<String> {
    NOTICE.lock().unwrap_or_else(PoisonError::into_inner).take()
}

/// yt-dlp, ready for arguments: ours while we hold one, else whatever `PATH` finds.
///
/// The day's check rides on the first call, so it costs nothing in a session that only
/// ever plays local files.
pub fn command() -> Command {
    ensure();
    Command::new(program())
}

/// Make sure the day's check has happened. For the paths that hand mpv a link without
/// running yt-dlp themselves: mpv's own hook needs one to exist before its first try.
pub fn ensure() {
    refresh(&|text| *NOTICE.lock().unwrap_or_else(PoisonError::into_inner) = Some(text));
}

/// [`ensure`] for the moment before the interface exists, when a line on the terminal is
/// still the right way to say what is taking so long. `false` when there is no yt-dlp to
/// run and none could be fetched.
pub fn ensure_announced() -> bool {
    refresh(&|text| eprintln!("{text}"));
    available()
}

/// `PATH` with our copy's directory in front, for mpv.
///
/// Its `ytdl_hook` looks `yt-dlp` up by name every time it runs one, so this is all it
/// takes for the hook to use the copy the player uses - including one fetched after that
/// mpv was started.
pub fn search_path() -> Option<OsString> {
    let ours = managed()?.parent()?.to_path_buf();
    let rest = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(std::iter::once(ours).chain(std::env::split_paths(&rest))).ok()
}

/// `$XDG_STATE_HOME/ytmplayer/bin/yt-dlp`. A directory of its own, because it goes on
/// mpv's `PATH` and nothing else in the state directory is a program.
fn managed() -> Option<PathBuf> {
    store::state_dir().map(|dir| dir.join("bin").join("yt-dlp"))
}

fn program() -> PathBuf {
    managed()
        .filter(|path| path.exists())
        .unwrap_or_else(|| PathBuf::from("yt-dlp"))
}

fn available() -> bool {
    program().is_absolute() || on_path("yt-dlp").is_some()
}

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// Run the check if it is due, once per process.
///
/// Blocking, on whichever thread asked first: a resolution that is about to be attempted
/// with a yt-dlp months out of date is better off waiting the second this takes. A caller
/// that arrives while another thread is mid-download does not queue behind it when there
/// is already something to run.
fn refresh(say: &dyn Fn(String)) {
    static DONE: Mutex<bool> = Mutex::new(false);
    let mut done = match DONE.try_lock() {
        Ok(done) => done,
        Err(_) if available() => return,
        Err(_) => DONE.lock().unwrap_or_else(PoisonError::into_inner),
    };
    if std::mem::replace(&mut *done, true) {
        return;
    }

    let stamp = store::state_dir().map(|dir| dir.join("yt-dlp.checked"));
    let checked_recently = stamp
        .as_ref()
        .and_then(|stamp| stamp.metadata().ok()?.modified().ok()?.elapsed().ok())
        .is_some_and(|age| age < CHECK_EVERY);
    // Nothing to run overrides the timetable: that is not a check, it is an install.
    if checked_recently && available() {
        return;
    }
    match update(say) {
        Ok(Some(version)) => say(format!("yt-dlp {version} installed")),
        Ok(None) => {}
        // Not worth a word while something still runs: an old yt-dlp that works is the
        // situation every day before today was.
        Err(_) if available() => {}
        Err(why) => say(format!("yt-dlp could not be fetched: {why}")),
    }
    // Stamped whatever happened, so a network that cannot reach GitHub costs one short
    // wait a day rather than one per launch.
    if let Some(stamp) = stamp {
        let _ = stamp.parent().map(std::fs::create_dir_all);
        let _ = std::fs::write(stamp, "");
    }
}

/// Bring the newest runnable yt-dlp up to the latest release. `Ok(Some(version))` when a
/// download was installed, `Ok(None)` when there was already nothing newer to have.
fn update(say: &dyn Fn(String)) -> Result<Option<String>, String> {
    let ours = managed().ok_or("no home directory to keep it in")?;
    let latest = latest_release()?;
    let system = on_path("yt-dlp").and_then(|path| version_of(&path));
    let mut held = version_of(&ours);
    // The distribution has caught up. Its copy starts faster and has its optional
    // dependencies wired in, so ours retires instead of shadowing it.
    if held.is_some() && system >= held {
        let _ = std::fs::remove_file(&ours);
        held = None;
    }
    if !outdated(system.max(held).as_deref(), &latest) {
        return Ok(None);
    }

    // Nothing can be verified without one, so nothing is fetched to be turned away.
    if !SHA256_PROGRAMS
        .iter()
        .any(|(program, _)| on_path(program).is_some())
    {
        return Err("sha256sum is needed to check the download and is not installed".into());
    }
    say(format!("Fetching yt-dlp {latest}…"));
    let dir = ours.parent().ok_or("no directory to keep it in")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let sums = release_sums(&latest)?;
    let temp = ours.with_extension(format!("{}.tmp", std::process::id()));
    for file in release_files() {
        let fetched = youtube::succeeds(
            Command::new("curl")
                .args(["-fsSL", "--proto", "=https", "--retry", "2"])
                // A stalled transfer is abandoned; a slow one that is still moving is not.
                .args(["--connect-timeout", "10", "--speed-limit", "1024"])
                .args(["--speed-time", "30", "-o"])
                .arg(&temp)
                .arg(format!("{RELEASES}/download/{latest}/{file}")),
        );
        let runnable = fetched
            // Before it is ever run: byte for byte the file the release lists. A download
            // that was cut short or mangled can still start and still say its version.
            && verified(&sums, file, &temp)
            && std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755)).is_ok()
            // Proof it is what was asked for and that it starts on this machine - a glibc
            // build on a musl system, or a zip with no Python new enough, fails here and
            // the next candidate gets its turn.
            && version_of(&temp).as_deref() == Some(latest.as_str());
        // Renamed into place, so a yt-dlp another thread is starting is never half a file.
        if runnable && std::fs::rename(&temp, &ours).is_ok() {
            return Ok(Some(latest));
        }
        let _ = std::fs::remove_file(&temp);
    }
    Err(format!(
        "no build of {latest} could be downloaded, verified and run here"
    ))
}

/// The release's `SHA2-256SUMS`: one `<digest>  <file>` line per file, as `sha256sum`
/// writes them.
fn release_sums(latest: &str) -> Result<String, String> {
    let output = Command::new("curl")
        .args(["-fsSL", "--proto", "=https", "--connect-timeout", "10"])
        .args(["--max-time", "30"])
        .arg(format!("{RELEASES}/download/{latest}/SHA2-256SUMS"))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| "curl is not installed".to_string())?;
    if !output.status.success() {
        return Err(format!("the checksums of {latest} could not be fetched"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// What can say a file's SHA-256, digest first: `sha256sum` wherever there are coreutils or
/// a busybox, and the `shasum` that comes with Perl - which is the one a Mac has always had.
const SHA256_PROGRAMS: [(&str, &[&str]); 2] = [("sha256sum", &[]), ("shasum", &["-a", "256"])];

/// Whether `download` is the file the release lists as `name` - the check yt-dlp's own
/// updater makes. A program rather than a hashing crate, for the reason it is `curl`
/// rather than a TLS one; with none of them here nothing is verified, so nothing is
/// installed.
fn verified(sums: &str, name: &str, download: &Path) -> bool {
    let digest = SHA256_PROGRAMS.iter().find_map(|(program, flags)| {
        let output = Command::new(program)
            .args(*flags)
            .arg(download)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        let printed = String::from_utf8_lossy(&output.stdout);
        printed.split_whitespace().next().map(str::to_string)
    });
    digest.is_some_and(|digest| {
        listed(sums, name).is_some_and(|published| published.eq_ignore_ascii_case(&digest))
    })
}

/// The digest `sums` lists for exactly `name` - `yt-dlp` is not `yt-dlp_linux`.
fn listed<'a>(sums: &'a str, name: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let (digest, file) = line.split_once("  ")?;
        (file == name).then_some(digest)
    })
}

/// Whether `latest` is newer than what is installed, `None` being nothing at all.
///
/// yt-dlp versions are zero-padded dates (`2026.10.26`, or `2026.10.26.232815` for a
/// nightly), so text order is release order and no version parser is needed.
fn outdated(installed: Option<&str>, latest: &str) -> bool {
    installed < Some(latest)
}

/// The tag of the latest release, read off where GitHub redirects `releases/latest` -
/// the one way to ask that is not rate-limited per address the way its API is.
fn latest_release() -> Result<String, String> {
    let output = Command::new("curl")
        .args(["-fsSLI", "--proto", "=https", "--connect-timeout", "5"])
        .args(["--max-time", "15", "-o", "/dev/null"])
        .args(["-w", "%{url_effective}"])
        .arg(format!("{RELEASES}/latest"))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| "curl is not installed".to_string())?;
    release_tag(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| "GitHub did not say which release is the latest".to_string())
}

/// `…/releases/tag/2026.10.26` -> `2026.10.26`.
///
/// Anything else - a login page, an error page, the URL that was asked for - is not an
/// answer. The tag goes into a download URL and a version comparison, so only something
/// shaped like a version gets through.
fn release_tag(url: &str) -> Option<String> {
    let tag = url.trim().rsplit_once("/releases/tag/")?.1;
    let versioned = tag.starts_with(|c: char| c.is_ascii_digit())
        && tag.bytes().all(|b| b.is_ascii_digit() || b == b'.');
    versioned.then(|| tag.to_string())
}

/// What `--version` says, when the file is a yt-dlp that starts on this machine.
fn version_of(program: &Path) -> Option<String> {
    let output = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && version.starts_with(|c: char| c.is_ascii_digit()))
        .then_some(version)
}

/// The release files that could run here, best first.
fn release_files() -> Vec<&'static str> {
    if cfg!(target_os = "macos") {
        return vec!["yt-dlp_macos"];
    }
    let mut files = Vec::new();
    // The zip is a tenth of the size and starts several times faster than the
    // self-extracting builds, but it needs a Python to run it.
    if on_path("python3").is_some() {
        files.push("yt-dlp");
    }
    // The player is linked statically, so its own target says nothing about the host's
    // libc. What does is whether the loader a glibc program starts through is there.
    let standalone = match std::env::consts::ARCH {
        "x86_64" => Some((
            "/lib64/ld-linux-x86-64.so.2",
            "yt-dlp_linux",
            "yt-dlp_musllinux",
        )),
        "aarch64" => Some((
            "/lib/ld-linux-aarch64.so.1",
            "yt-dlp_linux_aarch64",
            "yt-dlp_musllinux_aarch64",
        )),
        _ => None,
    };
    if let Some((loader, glibc, musl)) = standalone {
        files.push(if Path::new(loader).exists() {
            glibc
        } else {
            musl
        });
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_release_tag_is_an_answer() {
        assert_eq!(
            release_tag("https://github.com/yt-dlp/yt-dlp/releases/tag/2026.10.26\n").as_deref(),
            Some("2026.10.26")
        );
        // A 404 leaves the URL that was asked for; a proxy answers with its own page.
        for not_a_tag in [
            "https://github.com/yt-dlp/yt-dlp/releases/latest",
            "https://github.com/login?return_to=%2Freleases%2Ftag%2F",
            "https://example.com/releases/tag/../../evil",
            "https://github.com/yt-dlp/yt-dlp/releases/tag/",
            "",
        ] {
            assert_eq!(release_tag(not_a_tag), None, "{not_a_tag}");
        }
    }

    #[test]
    fn a_newer_release_outranks_whatever_is_installed() {
        assert!(outdated(None, "2026.10.26"), "nothing installed");
        assert!(outdated(Some("2025.01.15"), "2026.10.26"));
        assert!(outdated(Some("2026.09.30"), "2026.10.01"));
        assert!(!outdated(Some("2026.10.26"), "2026.10.26"));
        // A nightly build of the same day is already past the stable tag.
        assert!(!outdated(Some("2026.10.26.232815"), "2026.10.26"));
    }

    #[test]
    fn a_download_is_only_as_good_as_the_checksum_the_release_lists_for_it() {
        let sums = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  yt-dlp\n\
                    0000000000000000000000000000000000000000000000000000000000000000  yt-dlp_linux\n";
        // By the whole name - `yt-dlp` is not `yt-dlp_linux` - and a file the release does
        // not list, or an error page where the list should be, has nothing to match.
        assert_eq!(listed(sums, "yt-dlp_linux").map(|d| &d[..4]), Some("0000"));
        assert_eq!(listed(sums, "yt-dlp_musllinux"), None);
        assert_eq!(listed("<html>Not Found</html>", "yt-dlp"), None);

        let file = std::env::temp_dir().join(format!("ytmplayer-sums-{}", std::process::id()));
        std::fs::write(&file, "abc").unwrap();
        let listed_as_it_is = verified(sums, "yt-dlp", &file);
        let listed_as_something_else = verified(sums, "yt-dlp_linux", &file);
        let _ = std::fs::remove_file(&file);
        assert!(
            listed_as_it_is,
            "the file the release lists was turned away"
        );
        assert!(
            !listed_as_something_else,
            "a file with another digest got through"
        );
    }

    #[test]
    fn mpv_is_pointed_at_our_copy_first() {
        let Some(path) = search_path() else {
            return; // no home directory on this machine: nothing to point at
        };
        let first = std::env::split_paths(&path).next().unwrap_or_default();
        assert!(first.ends_with("ytmplayer/bin"), "{}", first.display());
    }
}
