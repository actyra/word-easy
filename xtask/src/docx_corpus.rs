//! `cargo xtask corpus docx [--dpi N] [--timeout S] [--maps] [--force] [--save-baseline]`:
//! a batch fidelity run over a local DOCX corpus (gap #1).
//!
//! The corpus itself is never committed: `corpus/` is gitignored, and documents from Word, Google
//! Docs, LibreOffice or Pages — and anything Word exported — stay out of the repository. The run
//! writes `corpus/docx/report.json`, which doubles as the manifest of what was checked.
//!
//! Layout, by naming convention rather than a hand-written manifest:
//!
//! ```text
//! corpus/docx/<stem>.docx        the document under test
//! corpus/docx/<stem>.ref.pdf     optional: the same document exported to PDF from Word
//! corpus/docx/out/<stem>/        our renders and per-document result.json
//! corpus/docx/report.json        the run, and the manifest of what it covered
//! corpus/docx/baseline.json      the run `--save-baseline` last recorded
//! ```
//!
//! Each document gets two tiers of checks:
//!
//! - **Tier A** needs no Word file and runs on every document: convert to PDF, round-trip
//!   `.docx` → `.docx`, and require the extracted text to survive that round trip. With poppler,
//!   the two renders are also compared page by page. A convert that fails, hangs or panics is
//!   recorded and the batch goes on, so one bad document can't end the run.
//! - **Tier B** runs only where a `<stem>.ref.pdf` sits beside the document, and compares our
//!   render with Word's page by page. Its headline is the count of documents whose page count
//!   matches Word exactly.
//!
//! Nothing here reports a failure merely because a document differs from Word — every document
//! does. The exit status is non-zero only for a harness error, a document that newly fails
//! outright, or a regression against the saved baseline.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::visual_diff;

/// How far a document's same-line-break share may fall before it counts as a regression.
const SAME_BREAK_TOLERANCE: f64 = 2.0;

/// Options for one run.
struct Opts {
    dpi: u32,
    timeout: Duration,
    maps: bool,
    force: bool,
    save_baseline: bool,
}

/// One document in the corpus, with the Word reference beside it when there is one.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Doc {
    stem: String,
    path: PathBuf,
    reference: Option<PathBuf>,
}

pub fn run(args: &[&str]) -> Result<(), String> {
    let opts = parse_opts(args)?;
    let dir = crate::root().join("corpus").join("docx");
    if !dir.is_dir() {
        print_setup(&dir);
        return Ok(());
    }
    let docs = pair_documents(&read_dir_sorted(&dir)?);
    if docs.is_empty() {
        print_setup(&dir);
        return Ok(());
    }
    let cli = cli_binary()?;
    let cli_print = fingerprint(&std::fs::read(&cli).map_err(|e| format!("{}: {e}", cli.display()))?);
    let baseline = read_json(&dir.join("baseline.json"));
    let previous = read_json(&dir.join("report.json"));
    let reuse = (!opts.force).then_some(previous.as_ref()).flatten().filter(|p| p["env"]["cli"] == json!(cli_print));

    println!("DOCX corpus: {} document(s) in {}", docs.len(), dir.display());
    match pdftotext_banner() {
        None => println!("note: poppler's pdftotext is not on PATH — pages will not be compared."),
        Some(b) => {
            if let Some(problem) = poppler_problem(&b) {
                println!("note: {problem}");
            }
        }
    }
    println!();
    let mut results = Vec::new();
    for doc in &docs {
        let print = match std::fs::read(&doc.path) {
            Ok(b) => fingerprint(&b),
            Err(e) => {
                results.push(failed(&doc.stem, "unreadable", &e.to_string()));
                continue;
            }
        };
        if let Some(done) = reuse.and_then(|p| find_doc(p, &doc.stem)).filter(|d| d["fingerprint"] == json!(print)) {
            println!("  {:<40} (unchanged)", doc.stem);
            results.push(done.clone());
            continue;
        }
        let row = check_document(doc, &print, &cli, &dir, &opts);
        println!("  {:<40} {}", doc.stem, one_line(&row));
        results.push(row);
    }

    let report = json!({
        "env": environment(&cli_print, opts.dpi),
        "summary": summarize(&results),
        "documents": results,
    });
    write_json(&dir.join("report.json"), &report)?;
    if opts.save_baseline {
        write_json(&dir.join("baseline.json"), &report)?;
        println!("\nbaseline saved");
    }
    finish(&report, baseline.as_ref(), &dir)
}

/// Print the run, then the regressions, and fail only on a hard problem.
fn finish(report: &Value, baseline: Option<&Value>, dir: &Path) -> Result<(), String> {
    println!("\n{}", summary_lines(&report["summary"]));
    println!("report: {}", dir.join("report.json").display());

    let broken: Vec<&Value> = report["documents"].as_array().map(|a| a.iter().filter(|d| d["status"] != json!("ok")).collect()).unwrap_or_default();
    if !broken.is_empty() {
        println!("\n{} document(s) did not convert cleanly:", broken.len());
        for d in &broken {
            println!("  {}: {} — {}", text_of(&d["stem"]), text_of(&d["status"]), text_of(&d["detail"]));
        }
    }
    let regressions = match baseline {
        Some(b) => classify(report, b),
        None => Vec::new(),
    };
    if !regressions.is_empty() {
        println!("\n{} regression(s) against baseline.json:", regressions.len());
        for r in &regressions {
            println!("  {r}");
        }
    } else if baseline.is_some() {
        println!("\nno regressions against baseline.json");
    }
    if !broken.is_empty() || !regressions.is_empty() {
        return Err(format!("{} failing document(s), {} regression(s)", broken.len(), regressions.len()));
    }
    Ok(())
}

// -------------------------------------------------------------------------------------------
// Per-document checks.

/// Tier A on every document, then Tier B where a Word reference sits beside it.
fn check_document(doc: &Doc, print: &str, cli: &Path, dir: &Path, opts: &Opts) -> Value {
    let out = dir.join("out").join(&doc.stem);
    if let Err(e) = std::fs::create_dir_all(&out) {
        return failed(&doc.stem, "harness", &format!("{}: {e}", out.display()));
    }
    let ours = out.join("ours.pdf");
    let round = out.join("roundtrip.docx");

    if let Err(e) = cli_run(cli, &[doc.path.as_os_str(), ours.as_os_str()], "convert", opts.timeout) {
        return failed(&doc.stem, e.kind, &e.detail);
    }
    if let Err(e) = cli_run(cli, &[doc.path.as_os_str(), round.as_os_str()], "convert", opts.timeout) {
        return failed(&doc.stem, e.kind, &e.detail);
    }
    let before = cli_text(cli, &doc.path, opts.timeout);
    let after = cli_text(cli, &round, opts.timeout);
    let (before, after) = match (before, after) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return failed(&doc.stem, e.kind, &e.detail),
    };

    let mut row = json!({
        "stem": doc.stem,
        "fingerprint": print,
        "status": "ok",
        "detail": "",
        "text_roundtrip": normalize(&before) == normalize(&after),
        "chars": [normalize(&before).chars().count(), normalize(&after).chars().count()],
    });
    if let Some(v) = round_trip_render(cli, &round, &out, opts) {
        row["roundtrip_render"] = v;
    }
    if let Some(reference) = &doc.reference {
        row["word"] = match compare_pdfs(reference, &ours, &out, opts) {
            Ok(v) => v,
            Err(e) => json!({"error": e}),
        };
    }
    row
}

/// Render the round-tripped copy and compare it with our render of the original. Absent poppler
/// this is skipped rather than failed: the text round trip above already ran.
fn round_trip_render(cli: &Path, round: &Path, out: &Path, opts: &Opts) -> Option<Value> {
    let rt_pdf = out.join("roundtrip.pdf");
    cli_run(cli, &[round.as_os_str(), rt_pdf.as_os_str()], "convert", opts.timeout).ok()?;
    let dir = out.join("roundtrip-diff");
    compare_pdfs(&out.join("ours.pdf"), &rt_pdf, &dir, opts).ok()
}

/// `visual_diff::compare` over a scratch directory it owns, reduced to the summary numbers.
fn compare_pdfs(reference: &Path, test: &Path, out: &Path, opts: &Opts) -> Result<Value, String> {
    std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let tmp = out.join(".tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    // Private unless `--maps`: a corpus holds other people's documents, so no text and no
    // page images leave the run by default.
    let result = visual_diff::compare(reference, test, out, &tmp, opts.dpi, !opts.maps);
    let _ = std::fs::remove_dir_all(&tmp);
    let report = result?;
    std::fs::write(out.join("result.json"), serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?)
        .map_err(|e| format!("{}: {e}", out.display()))?;
    let s = &report["summary"];
    let pages = s["pages"].as_array().cloned().unwrap_or_default();
    let (rp, tp) = (pages.first().and_then(Value::as_u64), pages.get(1).and_then(Value::as_u64));
    Ok(json!({
        "pages": [rp, tp],
        "pages_exact": rp.is_some() && rp == tp,
        "ssim_mean": s["ssim_mean"],
        "same_break_pct": s["same_break_pct"],
        "page_starts_same": s["page_starts_same"],
    }))
}

// -------------------------------------------------------------------------------------------
// Running the CLI.

/// Why a CLI call did not produce a usable result.
struct RunFail {
    kind: &'static str,
    detail: String,
}

/// Run `wordcraft-cli <sub> <args…>`, killing it after `timeout`.
///
/// Both pipes are drained by their own thread for as long as the child runs. Waiting on the
/// child while its output sits unread deadlocks as soon as a pipe buffer fills (~64 KiB), and a
/// harness built to find hangs must not invent one: that document would be killed by the timeout
/// and reported as hanging when it was the harness holding it still.
fn cli_run(cli: &Path, args: &[&std::ffi::OsStr], sub: &str, timeout: Duration) -> Result<Vec<u8>, RunFail> {
    let mut child = Command::new(cli)
        .arg(sub)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| RunFail { kind: "harness", detail: format!("{}: {e}", cli.display()) })?;
    let out_thread = drain(child.stdout.take());
    let err_thread = drain(child.stderr.take());
    // Killing the child closes both pipes, so the readers always end and the joins never block.
    let collect = |t: std::thread::JoinHandle<Vec<u8>>| t.join().unwrap_or_default();

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = (collect(out_thread), collect(err_thread));
                    return Err(RunFail { kind: "timeout", detail: format!("killed after {}s", timeout.as_secs()) });
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(RunFail { kind: "harness", detail: e.to_string() }),
        }
    };
    let (stdout, stderr) = (collect(out_thread), collect(err_thread));
    if status.success() {
        return Ok(stdout);
    }
    let err = String::from_utf8_lossy(&stderr).trim().chars().take(300).collect::<String>();
    // A panic escaping the CLI is the never-crash standard's concern, not an ordinary refusal,
    // so it is recorded under its own name.
    let kind = if err.contains("panicked at") { "panic" } else { "error" };
    Err(RunFail { kind, detail: if err.is_empty() { format!("exited with {status}") } else { err } })
}

/// Read one of the child's pipes to the end on its own thread, so a full pipe buffer can never
/// hold the child still while we wait for it.
fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    })
}

fn cli_text(cli: &Path, doc: &Path, timeout: Duration) -> Result<String, RunFail> {
    let out = cli_run(cli, &[doc.as_os_str()], "text", timeout)?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// The release CLI, under whichever target directory cargo is using.
fn cli_binary() -> Result<PathBuf, String> {
    let meta = crate::metadata()?;
    let target = meta["target_directory"].as_str().ok_or("cargo metadata has no target_directory")?;
    let path = Path::new(target).join("release").join(format!("wordcraft-cli{}", std::env::consts::EXE_SUFFIX));
    if path.is_file() {
        return Ok(path);
    }
    Err(format!("{} not found — run `cargo build --release -p wordcraft-cli` first", path.display()))
}

// -------------------------------------------------------------------------------------------
// Pure helpers: pairing, aggregation, regressions. These are what the tests below cover.

/// Pair each `<stem>.docx` with its `<stem>.ref.pdf` when one is present, in stem order.
/// `.ref.pdf` files and anything else in the directory are not documents under test.
pub(crate) fn pair_documents(entries: &[PathBuf]) -> Vec<Doc> {
    let has_ref = |stem: &str| entries.iter().any(|p| p.file_name().is_some_and(|n| n.to_string_lossy() == format!("{stem}.ref.pdf")));
    let mut docs: Vec<Doc> = entries
        .iter()
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("docx")))
        .filter_map(|p| {
            let stem = p.file_stem()?.to_string_lossy().into_owned();
            // `a.ref.pdf` has stem `a.ref`; a document never does, so this only guards odd names.
            (!stem.ends_with(".ref")).then(|| Doc {
                reference: has_ref(&stem).then(|| p.with_file_name(format!("{stem}.ref.pdf"))),
                stem,
                path: p.clone(),
            })
        })
        .collect();
    docs.sort_by(|a, b| a.stem.cmp(&b.stem));
    docs
}

/// The run's headline numbers. `pages_exact` over the documents that have a Word reference is the
/// figure gap #1 is measured by.
pub(crate) fn summarize(docs: &[Value]) -> Value {
    let ok = docs.iter().filter(|d| d["status"] == json!("ok")).count();
    let text_ok = docs.iter().filter(|d| d["text_roundtrip"] == json!(true)).count();
    let with_ref: Vec<&Value> = docs.iter().filter(|d| d["word"].is_object() && d["word"]["error"].is_null()).collect();
    let exact = with_ref.iter().filter(|d| d["word"]["pages_exact"] == json!(true)).count();
    json!({
        "documents": docs.len(),
        "converted": ok,
        "failed": docs.len() - ok,
        "text_roundtrip_ok": text_ok,
        "with_word_reference": with_ref.len(),
        "pages_exact": exact,
        "ssim_mean": mean(&with_ref, "ssim_mean"),
        "same_break_pct": mean(&with_ref, "same_break_pct"),
    })
}

/// Everything that got worse since the baseline. Differing from Word is never a regression:
/// only losing ground against what this repository itself last managed.
pub(crate) fn classify(report: &Value, baseline: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(docs) = report["documents"].as_array() else { return out };
    for d in docs {
        let stem = text_of(&d["stem"]);
        let Some(b) = find_doc(baseline, &stem) else { continue };
        if b["status"] == json!("ok") && d["status"] != json!("ok") {
            out.push(format!("{stem}: now {} ({})", text_of(&d["status"]), text_of(&d["detail"])));
        }
        if b["text_roundtrip"] == json!(true) && d["text_roundtrip"] == json!(false) {
            out.push(format!("{stem}: text no longer survives a docx round trip"));
        }
        if b["word"]["pages_exact"] == json!(true) && d["word"]["pages_exact"] == json!(false) {
            out.push(format!("{stem}: page count no longer matches Word ({} vs {})", pages_of(d), pages_of(b)));
        }
        let (now, was) = (d["word"]["same_break_pct"].as_f64(), b["word"]["same_break_pct"].as_f64());
        if let (Some(now), Some(was)) = (now, was)
            && was - now > SAME_BREAK_TOLERANCE
        {
            out.push(format!("{stem}: same line breaks {was:.1}% → {now:.1}%"));
        }
    }
    out
}

/// FNV-1a over the bytes, as `visual_diff::hash` does over text: stable across Rust versions,
/// which a `DefaultHasher` fingerprint would not be.
pub(crate) fn fingerprint(bytes: &[u8]) -> String {
    let h = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3));
    format!("{h:016x}")
}

/// Text compared across a round trip ignores trailing blanks and line-ending style only.
fn normalize(s: &str) -> String {
    s.replace("\r\n", "\n").trim_end().to_string()
}

fn mean(docs: &[&Value], key: &str) -> Value {
    let vals: Vec<f64> = docs.iter().filter_map(|d| d["word"][key].as_f64()).collect();
    if vals.is_empty() {
        return Value::Null;
    }
    json!((vals.iter().sum::<f64>() / vals.len() as f64 * 10_000.0).round() / 10_000.0)
}

/// A document's `[Word, ours]` page counts as `Word N → ours M`. Rust's `Debug` for a JSON value
/// would print `Array [Number(1), Number(2)]`, which is not something to show anyone.
fn pages_of(doc: &Value) -> String {
    let n = |i: usize| doc["word"]["pages"].get(i).and_then(Value::as_u64).map_or("?".to_string(), |v| v.to_string());
    format!("Word {} → ours {}", n(0), n(1))
}

fn find_doc<'a>(report: &'a Value, stem: &str) -> Option<&'a Value> {
    report["documents"].as_array()?.iter().find(|d| d["stem"] == json!(stem))
}

fn failed(stem: &str, kind: &str, detail: &str) -> Value {
    json!({"stem": stem, "status": kind, "detail": detail, "text_roundtrip": false})
}

fn text_of(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// One line per document in the run log.
fn one_line(row: &Value) -> String {
    if row["status"] != json!("ok") {
        return format!("{} — {}", text_of(&row["status"]), text_of(&row["detail"]));
    }
    let mut parts = vec![if row["text_roundtrip"] == json!(true) { "text ok".to_string() } else { "TEXT LOST".to_string() }];
    if let Some(w) = row["word"].as_object() {
        // A comparison that failed is not a fidelity result: saying "pages differ" here would
        // read as a measurement when nothing was measured.
        if let Some(e) = w.get("error").and_then(Value::as_str) {
            parts.push(format!("Word: not compared — {}", e.lines().next().unwrap_or(e)));
            return parts.join(", ");
        }
        let pages = if w.get("pages_exact") == Some(&json!(true)) { "pages exact" } else { "pages differ" };
        parts.push(format!("Word: {pages}"));
        if let Some(s) = w.get("same_break_pct").and_then(Value::as_f64) {
            parts.push(format!("{s:.1}% same breaks"));
        }
    }
    parts.join(", ")
}

/// Why the `pdftotext` on PATH can't drive a page-by-page comparison, read from its banner.
///
/// Git for Windows ships Xpdf's `pdftotext` in `mingw64/bin`, which has no `-bbox-layout` and so
/// prints its usage instead of a result. Without this the whole usage screen lands in the report
/// once per document, which reads as a fidelity problem rather than a PATH one.
/// Poppler is recognised by naming itself, never by the absence of Xpdf: poppler's own banner
/// credits "Glyph & Cog, LLC" too, so looking for that alone rejects the right tool.
pub(crate) fn poppler_problem(banner: &str) -> Option<&'static str> {
    (!banner.contains("Poppler") && !banner.contains("poppler")).then_some(
        "the `pdftotext` on PATH is not poppler's (Xpdf's has no `-bbox-layout`), so pages cannot be \
         compared. Git for Windows ships Xpdf's in mingw64/bin — put poppler's bin directory earlier on PATH.",
    )
}

/// The banner `pdftotext -v` prints, or None when it isn't on PATH at all.
fn pdftotext_banner() -> Option<String> {
    let out = Command::new("pdftotext").arg("-v").output().ok()?;
    Some(format!("{}{}", String::from_utf8_lossy(&out.stderr), String::from_utf8_lossy(&out.stdout)))
}

fn summary_lines(s: &Value) -> String {
    let n = |k: &str| s[k].as_u64().unwrap_or(0);
    let mut out = format!(
        "{} document(s): {} converted, {} failed, {} kept their text through a docx round trip",
        n("documents"),
        n("converted"),
        n("failed"),
        n("text_roundtrip_ok")
    );
    if n("with_word_reference") > 0 {
        out.push_str(&format!("\n{} with a Word reference: {} match Word's page count exactly", n("with_word_reference"), n("pages_exact")));
        if let Some(v) = s["same_break_pct"].as_f64() {
            out.push_str(&format!(", {v:.1}% of reference lines break in the same place"));
        }
        if let Some(v) = s["ssim_mean"].as_f64() {
            out.push_str(&format!(", mean SSIM {v:.4}"));
        }
    }
    out
}

// -------------------------------------------------------------------------------------------
// Run metadata, options and I/O.

/// What the numbers depend on. Renders use the installed fonts, so two runs only compare
/// fairly when these agree.
fn environment(cli_print: &str, dpi: u32) -> Value {
    json!({
        "commit": git_head(),
        "os": format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        "dpi": dpi,
        "poppler": poppler_version(),
        "craft_fonts": std::env::var_os("CRAFT_FONTS_DIR").is_some(),
        "cli": cli_print,
    })
}

fn git_head() -> Value {
    let out = Command::new("git").args(["rev-parse", "HEAD"]).current_dir(crate::root()).output();
    match out {
        Ok(o) if o.status.success() => json!(String::from_utf8_lossy(&o.stdout).trim()),
        _ => Value::Null,
    }
}

fn poppler_version() -> Value {
    // pdftoppm writes its banner to stderr.
    match Command::new("pdftoppm").arg("-v").output() {
        Ok(o) => {
            let text = format!("{}{}", String::from_utf8_lossy(&o.stderr), String::from_utf8_lossy(&o.stdout));
            match text.lines().next() {
                Some(l) if !l.trim().is_empty() => json!(l.trim()),
                _ => Value::Null,
            }
        }
        Err(_) => Value::Null,
    }
}

fn parse_opts(args: &[&str]) -> Result<Opts, String> {
    let number = |flag: &str, lo: u64, hi: u64| -> Result<Option<u64>, String> {
        match args.iter().position(|a| *a == flag) {
            Some(i) => {
                let v = args.get(i + 1).and_then(|v| v.parse::<u64>().ok()).filter(|v| (lo..=hi).contains(v));
                v.map(Some).ok_or_else(|| format!("{flag} takes {lo}..={hi}"))
            }
            None => Ok(None),
        }
    };
    for a in args.iter().filter(|a| a.starts_with("--")) {
        if !["--dpi", "--timeout", "--maps", "--force", "--save-baseline"].contains(a) {
            return Err(format!("unknown option `{a}`"));
        }
    }
    Ok(Opts {
        dpi: number("--dpi", 36, 300)?.unwrap_or(100) as u32,
        timeout: Duration::from_secs(number("--timeout", 1, 3600)?.unwrap_or(120)),
        maps: args.contains(&"--maps"),
        force: args.contains(&"--force"),
        save_baseline: args.contains(&"--save-baseline"),
    })
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten().map(|e| e.path()).collect();
    v.sort();
    Ok(v)
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_json(path: &Path, v: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

fn print_setup(dir: &Path) {
    println!(
        "No DOCX corpus yet. Put documents in {}:

  <stem>.docx       a real document — from Word, Google Docs, LibreOffice or Pages
  <stem>.ref.pdf    optional: that same document exported to PDF from Word

Everything under corpus/ is gitignored and must stay that way: these are other
people's documents, and files produced by Word are not ours to commit. Then:

  cargo build --release -p wordcraft-cli
  cargo xtask corpus docx --save-baseline    # record where we stand today
  cargo xtask corpus docx                    # after a change: what moved

Without a .ref.pdf a document is still worth having: it is converted, round-tripped
and checked for lost text, which needs no Word file at all.",
        dir.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(|n| PathBuf::from("corpus/docx").join(n)).collect()
    }

    #[test]
    fn pairs_documents_with_their_references() {
        let docs = pair_documents(&paths(&["b.docx", "a.docx", "a.ref.pdf", "notes.txt", "c.ref.pdf"]));
        let stems: Vec<&str> = docs.iter().map(|d| d.stem.as_str()).collect();
        // Sorted, .txt ignored, and a .ref.pdf with no document of its own is not a document.
        assert_eq!(stems, ["a", "b"]);
        assert_eq!(docs[0].reference, Some(PathBuf::from("corpus/docx").join("a.ref.pdf")));
        assert_eq!(docs[1].reference, None);
    }

    #[test]
    fn the_ref_suffix_is_reserved_even_on_a_docx() {
        // `.ref` belongs to the reference convention, so `a.ref.docx` is not a document under
        // test: taking it as one would give it the stem `a.ref` and a second row beside `a`.
        let docs = pair_documents(&paths(&["a.docx", "a.ref.docx"]));
        let stems: Vec<&str> = docs.iter().map(|d| d.stem.as_str()).collect();
        assert_eq!(stems, ["a"]);
    }

    #[test]
    fn uppercase_extension_is_still_a_document() {
        let docs = pair_documents(&paths(&["Report.DOCX"]));
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].stem, "Report");
    }

    #[test]
    fn fingerprint_is_stable_and_distinguishes_content() {
        assert_eq!(fingerprint(b"wordcraft"), fingerprint(b"wordcraft"));
        assert_ne!(fingerprint(b"wordcraft"), fingerprint(b"wordcraf"));
        // Pinned so a change of algorithm can't silently invalidate every saved fingerprint.
        assert_eq!(fingerprint(b""), "cbf29ce484222325");
    }

    fn doc_row(stem: &str, status: &str, text_ok: bool, pages: Option<(u64, u64)>, same_break: f64) -> Value {
        let mut v = json!({"stem": stem, "status": status, "detail": "", "text_roundtrip": text_ok});
        if let Some((a, b)) = pages {
            v["word"] = json!({"pages": [a, b], "pages_exact": a == b, "ssim_mean": 0.9, "same_break_pct": same_break});
        }
        v
    }

    #[test]
    fn summary_counts_only_documents_with_a_word_reference() {
        let docs = vec![
            doc_row("a", "ok", true, Some((10, 10)), 98.0),
            doc_row("b", "ok", true, Some((10, 11)), 90.0),
            doc_row("c", "ok", true, None, 0.0),
            doc_row("d", "timeout", false, None, 0.0),
        ];
        let s = summarize(&docs);
        assert_eq!(s["documents"], json!(4));
        assert_eq!(s["converted"], json!(3));
        assert_eq!(s["failed"], json!(1));
        assert_eq!(s["with_word_reference"], json!(2));
        assert_eq!(s["pages_exact"], json!(1));
        // Only the two with a reference average in.
        assert_eq!(s["same_break_pct"], json!(94.0));
    }

    #[test]
    fn differing_from_word_is_not_a_regression() {
        // Every document differs from Word; a run that is merely imperfect must stay silent.
        let base = json!({"documents": [doc_row("a", "ok", true, Some((10, 12)), 80.0)]});
        let now = json!({"documents": [doc_row("a", "ok", true, Some((10, 12)), 80.0)]});
        assert!(classify(&now, &base).is_empty());
    }

    #[test]
    fn losing_ground_against_the_baseline_is_a_regression() {
        let base = json!({"documents": [doc_row("a", "ok", true, Some((10, 10)), 98.0)]});
        let now = json!({"documents": [doc_row("a", "ok", true, Some((10, 11)), 98.0)]});
        let out = classify(&now, &base);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].contains("page count no longer matches Word"), "{out:?}");
        // Readable numbers, not serde_json's Debug shape.
        assert!(out[0].contains("Word 10 → ours 11"), "{out:?}");
        assert!(!out[0].contains("Number("), "{out:?}");
    }

    #[test]
    fn a_new_crash_or_timeout_is_a_regression() {
        let base = json!({"documents": [doc_row("a", "ok", true, None, 0.0)]});
        let now = json!({"documents": [doc_row("a", "panic", false, None, 0.0)]});
        let out = classify(&now, &base);
        assert!(out.iter().any(|r| r.contains("now panic")), "{out:?}");
    }

    #[test]
    fn lost_text_in_a_round_trip_is_a_regression() {
        let base = json!({"documents": [doc_row("a", "ok", true, None, 0.0)]});
        let now = json!({"documents": [doc_row("a", "ok", false, None, 0.0)]});
        let out = classify(&now, &base);
        assert!(out.iter().any(|r| r.contains("text no longer survives")), "{out:?}");
    }

    #[test]
    fn small_line_break_drift_is_tolerated_and_a_real_drop_is_not() {
        let base = json!({"documents": [doc_row("a", "ok", true, Some((9, 9)), 98.0)]});
        let ok = json!({"documents": [doc_row("a", "ok", true, Some((9, 9)), 96.5)]});
        assert!(classify(&ok, &base).is_empty(), "1.5 points is within tolerance");
        let bad = json!({"documents": [doc_row("a", "ok", true, Some((9, 9)), 95.0)]});
        assert_eq!(classify(&bad, &base).len(), 1);
    }

    #[test]
    fn a_document_absent_from_the_baseline_is_not_a_regression() {
        let base = json!({"documents": []});
        let now = json!({"documents": [doc_row("new", "error", false, None, 0.0)]});
        assert!(classify(&now, &base).is_empty());
    }

    #[test]
    fn options_are_validated() {
        assert!(parse_opts(&["--dpi", "600"]).is_err());
        assert!(parse_opts(&["--timeout", "0"]).is_err());
        assert!(parse_opts(&["--nope"]).is_err());
        let o = parse_opts(&["--dpi", "150", "--force"]).expect("valid");
        assert_eq!(o.dpi, 150);
        assert_eq!(o.timeout, Duration::from_secs(120));
        assert!(o.force && !o.maps && !o.save_baseline);
    }

    #[test]
    fn xpdfs_pdftotext_is_recognised_and_popplers_is_accepted() {
        // Both banners are the real output of `pdftotext -v`, not a paraphrase: poppler credits
        // Glyph & Cog as well, so a check for that string alone rejects poppler itself.
        let xpdf = "pdftotext version 4.00\nCopyright 1996-2017 Glyph & Cog, LLC";
        let poppler = "pdftotext version 25.07.0\n\
                       Copyright 2005-2025 The Poppler Developers - http://poppler.freedesktop.org\n\
                       Copyright 1996-2011, 2022 Glyph & Cog, LLC";
        assert!(poppler_problem(xpdf).is_some(), "Xpdf must be rejected");
        assert!(poppler_problem(poppler).is_none(), "poppler must be accepted despite crediting Glyph & Cog");
    }

    #[test]
    fn a_failed_comparison_is_not_reported_as_a_fidelity_result() {
        let row = json!({"stem": "a", "status": "ok", "text_roundtrip": true, "word": {"error": "pdftotext failed: usage…\nmore"}});
        let line = one_line(&row);
        assert!(line.contains("not compared"), "{line}");
        // "pages differ" would read as a measurement when nothing was measured.
        assert!(!line.contains("pages differ"), "{line}");
        // Only the first line of a multi-line tool error reaches the log.
        assert!(!line.contains("more"), "{line}");
    }

    #[test]
    fn text_comparison_ignores_line_endings_and_trailing_blanks() {
        assert_eq!(normalize("a\r\nb\n\n"), normalize("a\nb"));
        assert_ne!(normalize("a b"), normalize("ab"));
    }
}
