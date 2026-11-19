// Embeds the version resource and application manifest (res/blink.rc).
// A proper VERSIONINFO + manifest makes Blink look like the ordinary
// desktop tool it is (Defender/SmartScreen weigh missing metadata).
fn main() {
    // Build ordinal: a larger number is a newer build (the Task Manager launcher is refreshed forward only).
    // Reproducible: BLINK_BUILD_ORDINAL (tests), else SOURCE_DATE_EPOCH, else the commit time of git HEAD,
    // never the wall clock of the build. When none of those exists (no git, no variable) it falls back to
    // the wall clock WITH a build warning, so a refresh is never locked out by an ordinal of 0.
    println!("cargo:rerun-if-env-changed=BLINK_BUILD_ORDINAL");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    let env_num = |n: &str| std::env::var(n).ok().and_then(|v| v.trim().parse::<u64>().ok());
    let git = |args: &[&str]| std::process::Command::new("git").args(args).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    // a new commit moves the loose ref file of the branch (or HEAD itself when detached): rebuild then
    for what in [Some("HEAD".to_string()), git(&["symbolic-ref", "-q", "HEAD"])].into_iter().flatten() {
        if let Some(path) = git(&["rev-parse", "--git-path", &what]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let ord = env_num("BLINK_BUILD_ORDINAL")
        .or_else(|| env_num("SOURCE_DATE_EPOCH"))
        .or_else(|| git(&["log", "-1", "--format=%ct"]).and_then(|t| t.parse::<u64>().ok()))
        .unwrap_or_else(|| {
            println!("cargo:warning=no git commit time and no SOURCE_DATE_EPOCH: the build ordinal is the wall clock of this build");
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(1)
        });
    println!("cargo:rustc-env=BLINK_BUILD={ord}");
    println!("cargo:rerun-if-changed=res/blink.rc");
    println!("cargo:rerun-if-changed=res/blink.manifest");
    println!("cargo:rerun-if-changed=res/blink.ico");
    embed_resource::compile("res/blink.rc", embed_resource::NONE).manifest_required().unwrap();
}
