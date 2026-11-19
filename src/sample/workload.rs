//! What a runtime process is running, read from its command line alone: the
//! jar or main class of a `java`, the script or package of a `node`, the entry
//! of a `bun`, the module or script of a `python`, the assembly of a `dotnet`.
//!
//! Pure text parsing of the string Blink already holds. Nothing is executed,
//! no file is opened and no path is resolved. A command line that is unknown
//! or ambiguous (inline code, an argument file, a package script name, a flag
//! whose value cannot be told from a positional) gives `None`, so the row
//! stays the plain runtime: no application identity is ever invented.

/// An inferred workload. Always derived, never read from the process itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workload {
    /// short, descriptive: jar file, main class, script, package, module, assembly
    pub label: String,
    /// "Java", "Node.js", "Bun", "Python", ".NET"
    pub runtime: &'static str,
    /// what `detail` is: "Java main class", "Node.js package", ...
    pub what: &'static str,
    /// the value as it appears on the command line (class name, script path, ...)
    pub detail: String,
}

/// File name of an image path or process name, lower case, without ".exe".
fn stem(exe: &str) -> String {
    let name = exe.rsplit(['\\', '/']).next().unwrap_or(exe).to_ascii_lowercase();
    name.strip_suffix(".exe").map(str::to_owned).unwrap_or(name)
}

/// The runtime an executable is, when it is one of the supported ones.
pub fn runtime_of(exe: &str) -> Option<&'static str> {
    let s = stem(exe);
    match s.as_str() {
        "java" | "javaw" => Some("Java"),
        "node" => Some("Node.js"),
        "bun" | "bunx" => Some("Bun"),
        "python" | "pythonw" | "py" | "pyw" => Some("Python"),
        "dotnet" => Some(".NET"),
        _ if is_python_versioned(&s) => Some("Python"),
        _ => None,
    }
}

/// python3, python3.12, pythonw3.11 ...
fn is_python_versioned(s: &str) -> bool {
    let rest = s.strip_prefix("pythonw").or_else(|| s.strip_prefix("python"));
    rest.is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit() || c == '.'))
}

/// Split a Windows command line into arguments the way the C runtime does
/// (`CommandLineToArgvW`): argv[0] only honours quotes; later arguments use the
/// 2n / 2n+1 backslash rule before a quote, and `""` inside a quoted run is a
/// literal quote. Text only; nothing is run or expanded.
pub fn split_args(cmd: &str) -> Vec<String> {
    let c: Vec<char> = cmd.chars().collect();
    let n = c.len();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < n && (c[i] == ' ' || c[i] == '\t') {
        i += 1;
    }
    if i >= n {
        return out;
    }
    // argv[0]: quotes toggle, no escapes
    let mut a0 = String::new();
    if c[i] == '"' {
        i += 1;
        while i < n && c[i] != '"' {
            a0.push(c[i]);
            i += 1;
        }
        i += 1;
    } else {
        while i < n && c[i] != ' ' && c[i] != '\t' {
            a0.push(c[i]);
            i += 1;
        }
    }
    out.push(a0);
    loop {
        while i < n && (c[i] == ' ' || c[i] == '\t') {
            i += 1;
        }
        if i >= n {
            break;
        }
        let mut arg = String::new();
        let mut inq = false;
        while i < n {
            let ch = c[i];
            if ch == '\\' {
                let mut bs = 0;
                while i < n && c[i] == '\\' {
                    bs += 1;
                    i += 1;
                }
                if i < n && c[i] == '"' {
                    arg.extend(std::iter::repeat_n('\\', bs / 2));
                    if bs % 2 == 1 {
                        arg.push('"');
                        i += 1;
                    }
                    // an even run leaves the quote to toggle below
                } else {
                    arg.extend(std::iter::repeat_n('\\', bs));
                }
                continue;
            }
            if ch == '"' {
                if inq && i + 1 < n && c[i + 1] == '"' {
                    arg.push('"');
                    i += 2;
                    continue;
                }
                inq = !inq;
                i += 1;
                continue;
            }
            if !inq && (ch == ' ' || ch == '\t') {
                break;
            }
            arg.push(ch);
            i += 1;
        }
        out.push(arg);
    }
    out
}

/// Infer the workload of the process whose image is `exe` (path or name) and
/// whose command line is `cmd`. `None` for anything that is not clearly one.
pub fn infer(exe: &str, cmd: &str) -> Option<Workload> {
    let runtime = runtime_of(exe)?;
    let s = stem(exe);
    let args = split_args(cmd);
    let args = args.get(1..)?;
    match runtime {
        "Java" => java(args),
        "Node.js" => node(args),
        "Bun" => bun(args, s == "bunx"),
        "Python" => python(args),
        ".NET" => dotnet(args),
        _ => None,
    }
}

fn w(runtime: &'static str, what: &'static str, label: String, detail: &str) -> Option<Workload> {
    if label.is_empty() {
        return None;
    }
    Some(Workload { label, runtime, what, detail: detail.to_string() })
}

/// Last component of a path-like argument (either separator).
fn file_name(p: &str) -> &str {
    p.trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next().unwrap_or(p)
}

fn java(args: &[String]) -> Option<Workload> {
    const VALUE: &[&str] = &[
        "-cp", "-classpath", "--class-path", "-p", "--module-path", "--upgrade-module-path", "--add-modules", "--add-opens", "--add-exports", "--add-reads",
        "--limit-modules", "--patch-module", "--enable-native-access",
    ];
    const FLAG: &[&str] = &["--enable-preview", "--show-version", "--dry-run", "--help", "--version", "--list-modules"];
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "-jar" {
            let jar = args.get(i + 1)?;
            return w("Java", "Java jar", file_name(jar).to_string(), jar);
        }
        if a == "-m" || a == "--module" {
            let m = args.get(i + 1)?;
            let module = m.split('/').next().unwrap_or(m);
            return w("Java", "Java module", module.to_string(), m);
        }
        if a.starts_with('@') {
            return None; // argument file: the real options are not on the command line
        }
        if a.starts_with('-') {
            if VALUE.contains(&a) {
                i += 2;
                continue;
            }
            // an unknown "--option value" cannot be told from "--option" + main class
            if a.starts_with("--") && !a.contains('=') && !FLAG.contains(&a) {
                return None;
            }
            i += 1;
            continue;
        }
        // first positional: the main class
        let ok = !a.is_empty() && a.chars().all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '$' | '/'));
        return if ok && !a.starts_with('.') { w("Java", "Java main class", a.to_string(), a) } else { None };
    }
    None
}

const SCRIPT_EXT: &[&str] = &["js", "mjs", "cjs", "ts", "mts", "cts", "jsx", "tsx"];

fn is_script(p: &str) -> bool {
    let n = file_name(p);
    n.rsplit_once('.').is_some_and(|(b, e)| !b.is_empty() && SCRIPT_EXT.contains(&e.to_ascii_lowercase().as_str()))
}

/// "pkg" or "@scope/pkg" from a path inside `node_modules`, else None.
fn package_of(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split(['\\', '/']).filter(|s| !s.is_empty()).collect();
    let i = parts.iter().rposition(|p| p.eq_ignore_ascii_case("node_modules"))?;
    let first = *parts.get(i + 1)?;
    if first == ".bin" {
        return None;
    }
    if first.starts_with('@') {
        let second = parts.get(i + 2)?;
        return Some(format!("{first}/{second}"));
    }
    // the path must continue to a file inside the package
    (parts.len() > i + 2).then(|| first.to_string())
}

/// "file.js", or "dir/file.js" when the file name alone says nothing.
fn script_label(path: &str) -> String {
    let parts: Vec<&str> = path.split(['\\', '/']).filter(|s| !s.is_empty() && *s != ".").collect();
    let file = parts.last().copied().unwrap_or(path);
    let base = file.rsplit_once('.').map(|(b, _)| b).unwrap_or(file).to_ascii_lowercase();
    const GENERIC: &[&str] = &["index", "main", "server", "app", "cli", "start", "run", "bin", "dist", "entry"];
    if GENERIC.contains(&base.as_str()) && parts.len() >= 2 {
        let dir = parts[parts.len() - 2];
        if dir != ".." && !dir.ends_with(':') {
            return format!("{dir}/{file}");
        }
    }
    file.to_string()
}

fn script_workload(runtime: &'static str, script: &str, what_script: &'static str, what_pkg: &'static str) -> Option<Workload> {
    if !is_script(script) {
        return None; // "node server" / "bun run dev": a name, not a file: ambiguous
    }
    match package_of(script) {
        Some(pkg) => w(runtime, what_pkg, pkg, script),
        None => w(runtime, what_script, script_label(script), script),
    }
}

fn node(args: &[String]) -> Option<Workload> {
    const VALUE: &[&str] = &["-r", "--require", "--import", "--loader", "--experimental-loader", "--conditions", "-C", "--input-type", "--title", "--max-old-space-size"];
    const INLINE: &[&str] = &["-e", "--eval", "-p", "--print", "-i", "--interactive", "-c", "--check", "--test", "-"];
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let flag = a.split('=').next().unwrap_or(a);
        if INLINE.contains(&flag) || ["-e", "-p"].iter().any(|p| a.starts_with(p) && !a.starts_with("--")) {
            return None;
        }
        if a.starts_with('-') {
            if VALUE.contains(&flag) {
                i += if a.contains('=') { 1 } else { 2 };
            } else if ["--inspect", "--inspect-brk", "--enable-source-maps", "--no-warnings", "--trace-warnings"].contains(&flag) {
                i += 1;
            } else {
                return None;
            }
            continue;
        }
        return script_workload("Node.js", a, "Node.js script", "Node.js package");
    }
    None
}

fn bun(args: &[String], bunx: bool) -> Option<Workload> {
    const VALUE: &[&str] = &["--cwd", "-c", "--config", "--port", "--env-file", "-r", "--preload", "--tsconfig-override", "--define", "-d", "--loader", "-l", "--install", "--jsx-factory"];
    const SUB: &[&str] = &[
        "install", "i", "add", "a", "remove", "rm", "update", "upgrade", "test", "build", "create", "c", "init", "link", "unlink", "pm", "dev", "repl", "exec", "publish", "patch",
        "outdated", "info", "why", "audit", "completions", "help", "feedback",
    ];
    let mut i = 0;
    let mut pos: Vec<&str> = Vec::new();
    while i < args.len() {
        let a = args[i].as_str();
        if a.starts_with('-') && a != "-" {
            let flag = a.split('=').next().unwrap_or(a);
            if ["-e", "--eval", "-p", "--print"].contains(&flag) || ["-e", "-p"].iter().any(|p| a.starts_with(p) && !a.starts_with("--")) {
                return None;
            }
            if VALUE.contains(&flag) {
                i += if a.contains('=') { 1 } else { 2 };
            } else if ["--hot", "--watch", "--smol", "--bun", "--silent", "--inspect", "--inspect-brk", "--inspect-wait"].contains(&flag) {
                i += 1;
            } else {
                return None;
            }
            continue;
        }
        pos.push(a);
        i += 1;
        if pos.len() == 2 || bunx || !["run", "x"].contains(&a) {
            break;
        }
    }
    let name = |pkg: &str| {
        // "pkg@latest", "@scope/pkg@1": the version is not part of the name
        let (head, tail) = pkg.strip_prefix('@').map(|r| ("@", r)).unwrap_or(("", pkg));
        format!("{head}{}", tail.split('@').next().unwrap_or(tail))
    };
    let first = *pos.first()?;
    if bunx {
        return w("Bun", "Bun package", name(first), first);
    }
    match first {
        "x" => {
            let p = *pos.get(1)?;
            w("Bun", "Bun package", name(p), p)
        }
        "run" => script_workload("Bun", pos.get(1)?, "Bun entry", "Bun package"),
        f if SUB.contains(&f) => None,
        f => script_workload("Bun", f, "Bun entry", "Bun package"),
    }
}

fn python(args: &[String]) -> Option<Workload> {
    const VALUE: &[&str] = &["-X", "-W", "-Q"];
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "-c" || a == "-" || a.starts_with("-c") && a.len() > 2 && !a.starts_with("--") {
            return None; // inline code or stdin
        }
        if a == "-m" {
            let m = args.get(i + 1)?;
            return w("Python", "Python module", m.clone(), m);
        }
        if let Some(m) = a.strip_prefix("-m").filter(|m| !m.is_empty() && !m.starts_with('-')) {
            return w("Python", "Python module", m.to_string(), m);
        }
        if a.starts_with('-') {
            i += if VALUE.contains(&a) { 2 } else { 1 };
            continue;
        }
        let ext = file_name(a).rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
        return match ext.as_deref() {
            Some("py" | "pyw" | "pyz") => w("Python", "Python script", file_name(a).to_string(), a),
            _ => None,
        };
    }
    None
}

fn dotnet(args: &[String]) -> Option<Workload> {
    const VALUE: &[&str] = &["--fx-version", "--roll-forward", "--additionalprobingpath", "--depsfile", "--runtimeconfig", "--additional-deps"];
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "exec" {
            i += 1;
            continue;
        }
        if a.starts_with('-') {
            i += if VALUE.contains(&a) { 2 } else { 1 };
            continue;
        }
        let f = file_name(a);
        return match f.rsplit_once('.') {
            Some((b, e)) if e.eq_ignore_ascii_case("dll") && !b.is_empty() => w(".NET", ".NET assembly", b.to_string(), a),
            _ => None, // "dotnet run" / "watch" / "test": which project is not on the command line
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wl(exe: &str, cmd: &str) -> Option<(String, &'static str, &'static str)> {
        infer(exe, cmd).map(|w| (w.label, w.runtime, w.what))
    }
    fn some(label: &str, runtime: &'static str, what: &'static str) -> Option<(String, &'static str, &'static str)> {
        Some((label.to_string(), runtime, what))
    }

    #[test]
    fn windows_quoting_follows_the_c_runtime() {
        let a = split_args(r#""C:\Program Files\Java\bin\javaw.exe" -Dx="a b" "C:\x y\" z"#);
        assert_eq!(a, vec![r"C:\Program Files\Java\bin\javaw.exe", "-Dx=a b", "C:\\x y\" z"]);
        // 2n backslashes then a quote: n backslashes, the quote toggles
        assert_eq!(split_args(r#"a.exe "b\\" c"#), vec!["a.exe", r"b\", "c"]);
        // 2n+1: literal quote
        assert_eq!(split_args(r#"a.exe b\"c d"#), vec!["a.exe", "b\"c", "d"]);
        // argv[0] has no escapes: the backslash before the closing quote is a path
        assert_eq!(split_args(r#""C:\dir\" x"#), vec![r"C:\dir\", "x"]);
        assert_eq!(split_args("   "), Vec::<String>::new());
        assert_eq!(split_args("java.exe\t-jar\tx.jar"), vec!["java.exe", "-jar", "x.jar"]);
        // doubled quote inside a quoted run is a literal quote
        assert_eq!(split_args(r#"a.exe "say ""hi""""#), vec!["a.exe", r#"say "hi""#]);
    }

    #[test]
    fn runtimes_are_recognised_by_image_name_only() {
        for (exe, rt) in [("javaw.exe", "Java"), (r"C:\x\JAVA.EXE", "Java"), ("node.exe", "Node.js"), ("bun.exe", "Bun"), ("python3.12.exe", "Python"), ("dotnet", ".NET"), ("py.exe", "Python")] {
            assert_eq!(runtime_of(exe), Some(rt), "{exe}");
        }
        for exe in ["javac.exe", "nodemon.exe", "pythonista.exe", "notepad.exe", "python-config.exe", "bunker.exe"] {
            assert_eq!(runtime_of(exe), None, "{exe}");
        }
        assert_eq!(infer("notepad.exe", "notepad.exe a.js"), None);
    }

    #[test]
    fn java_jar_main_class_and_module() {
        assert_eq!(wl("java.exe", r#""C:\j\bin\java.exe" -Xmx4G -Dfoo=bar -jar "C:\srv\paper 1.21.jar" nogui"#), some("paper 1.21.jar", "Java", "Java jar"));
        assert_eq!(
            wl("javaw.exe", r#"javaw.exe -Xmx16G -XX:+UseG1GC -Djava.library.path=C:\n -cp "a;b;c" -p mods --add-modules ALL net.minecraft.client.main.Main --version 1.21"#),
            some("net.minecraft.client.main.Main", "Java", "Java main class")
        );
        assert_eq!(wl("java.exe", "java --module-path m -m com.acme.app/com.acme.Main x"), some("com.acme.app", "Java", "Java module"));
        // a flag after the class is the program's, not Java's
        assert_eq!(wl("java.exe", "java a.B -jar x.jar"), some("a.B", "Java", "Java main class"));
    }

    #[test]
    fn java_ambiguity_is_not_guessed() {
        assert_eq!(wl("java.exe", "java @args.txt"), None, "argument file");
        assert_eq!(wl("java.exe", "java --unknown-opt value a.B"), None, "unknown separate value");
        assert_eq!(wl("java.exe", "java -version"), None);
        assert_eq!(wl("java.exe", "java"), None);
        assert_eq!(wl("java.exe", r"java C:\x\Main.class"), None, "a path is not a class name");
        assert_eq!(wl("java.exe", "java -cp"), None);
        assert_eq!(wl("java.exe", "java -jar"), None);
    }

    #[test]
    fn node_script_and_package() {
        assert_eq!(wl("node.exe", r#""C:\Program Files\nodejs\node.exe" --max-old-space-size=4096 C:\proj\api\dist\server.js --port 80"#), some("dist/server.js", "Node.js", "Node.js script"));
        assert_eq!(wl("node.exe", r"node C:\w\blink-web\node_modules\vite\bin\vite.js dev"), some("vite", "Node.js", "Node.js package"));
        assert_eq!(wl("node.exe", r"node -r ts-node/register C:\w\node_modules\@anthropic-ai\claude-code\cli.js"), some("@anthropic-ai/claude-code", "Node.js", "Node.js package"));
        assert_eq!(wl("node.exe", r"node C:\w\node_modules\.bin\tsc.js"), some("tsc.js", "Node.js", "Node.js script"));
        assert_eq!(wl("node.exe", r"node C:\tools\build-things.mjs"), some("build-things.mjs", "Node.js", "Node.js script"));
        assert_eq!(wl("node.exe", "node ./index.js"), some("index.js", "Node.js", "Node.js script"), "no folder to name it by");
        assert_eq!(wl("node.exe", "node proj/index.js"), some("proj/index.js", "Node.js", "Node.js script"));
    }

    #[test]
    fn node_ambiguity_is_not_guessed() {
        for runtime in ["node.exe", "bun.exe"] {
            for args in ["--eval=code worker.js", "--print=code worker.js", "-ecode worker.js", "--unknown service.js real.js"] {
                assert_eq!(wl(runtime, &format!("{runtime} {args}")), None);
            }
        }
        assert_eq!(wl("node.exe", "node --title service.js real.js"), some("real.js", "Node.js", "Node.js script"));
        for c in ["node", "node -e \"console.log(1)\"", "node -p 1", "node --inspect", "node server", "node --foo bar", "node -i", "node -"] {
            assert_eq!(wl("node.exe", c), None, "{c}");
        }
    }

    #[test]
    fn bun_entry_package_and_scripts() {
        assert_eq!(wl("bun.exe", r"bun run server.ts"), some("server.ts", "Bun", "Bun entry"), "bare name stays the file name");
        assert_eq!(wl("bun.exe", r"bun --hot C:\w\app\src\main.tsx"), some("src/main.tsx", "Bun", "Bun entry"));
        assert_eq!(wl("bun.exe", r"bun --cwd C:\w run index.ts"), some("index.ts", "Bun", "Bun entry"));
        assert_eq!(wl("bunx.exe", "bunx prettier@3 --check ."), some("prettier", "Bun", "Bun package"));
        assert_eq!(wl("bun.exe", "bun x @scope/tool@latest"), some("@scope/tool", "Bun", "Bun package"));
        for c in ["bun run dev", "bun install", "bun test", "bun", "bun run", "bun dev"] {
            assert_eq!(wl("bun.exe", c), None, "{c}");
        }
    }

    #[test]
    fn python_and_dotnet() {
        assert_eq!(wl("python.exe", r"python -u C:\a\worker.py --x"), some("worker.py", "Python", "Python script"));
        assert_eq!(wl("python3.12.exe", "python3.12 -m http.server 8000"), some("http.server", "Python", "Python module"));
        assert_eq!(wl("py.exe", "py -3 -X utf8 tool.py"), some("tool.py", "Python", "Python script"));
        assert_eq!(wl("python.exe", "python -c print(1)"), None);
        assert_eq!(wl("python.exe", "python"), None);
        assert_eq!(wl("python.exe", "python script"), None, "no extension: not a file name");
        assert_eq!(wl("dotnet.exe", r"dotnet exec C:\app\Contoso.Web.dll --urls x"), some("Contoso.Web", ".NET", ".NET assembly"));
        assert_eq!(wl("dotnet.exe", r"dotnet --roll-forward Major C:\app\Svc.dll"), some("Svc", ".NET", ".NET assembly"));
        assert_eq!(wl("dotnet.exe", "dotnet run"), None);
        assert_eq!(wl("dotnet.exe", "dotnet watch"), None);
    }

    #[test]
    fn nothing_is_run_or_read() {
        // hostile text is only ever text
        assert_eq!(wl("node.exe", "node ../../..\\evil.js && calc.exe"), some("evil.js", "Node.js", "Node.js script"));
        // a huge command line is parsed as text, once
        assert_eq!(infer("java.exe", &"x ".repeat(100_000)).map(|w| w.label), Some("x".to_string()));
    }
}
