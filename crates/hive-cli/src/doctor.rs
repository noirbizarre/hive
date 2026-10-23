//! Rendering for `hive doctor`; the checks themselves live in `hive_daemon::doctor`.

use std::io::Write;

use hive_daemon::doctor::{Finding, Report, Status};
use miette::{MietteDiagnostic, Severity};

fn marker(status: Status) -> &'static str {
    match status {
        Status::Pass => "ok",
        Status::Warn => "warn",
        Status::Fail => "FAIL",
        Status::Skip => "skip",
    }
}

/// One finding as text. Anything that is not a pass goes through `miette`, so the code and
/// the help text are laid out like every other Hive diagnostic.
fn render_finding(finding: &Finding, out: &mut String) {
    if finding.status == Status::Pass || finding.code.is_none() {
        out.push_str(&format!(
            "  [{}] {}\n",
            marker(finding.status),
            finding.message
        ));
        return;
    }
    let mut diagnostic = MietteDiagnostic::new(finding.message.clone()).with_severity(
        if finding.status == Status::Warn {
            Severity::Warning
        } else {
            Severity::Error
        },
    );
    if let Some(code) = &finding.code {
        diagnostic = diagnostic.with_code(code.clone());
    }
    if let Some(help) = &finding.help {
        diagnostic = diagnostic.with_help(help.clone());
    }
    out.push_str(&format!(
        "  [{}]\n{:?}\n",
        marker(finding.status),
        miette::Report::new(diagnostic)
    ));
}

fn section(title: &str, status: Status, findings: &[Finding], out: &mut String) {
    out.push_str(&format!("{title} [{}]\n", marker(status)));
    for finding in findings {
        render_finding(finding, out);
    }
    out.push('\n');
}

pub fn render(report: &Report) -> String {
    let mut out = String::new();
    section(
        &format!("Configuration ({})", report.config.path),
        report.config.status,
        &report.config.findings,
        &mut out,
    );
    for profile in &report.profiles {
        section(
            &format!(
                "Agent `{}` ({}, {})",
                profile.id, profile.kind, profile.transport
            ),
            profile.status,
            &profile.findings,
            &mut out,
        );
    }
    section(
        &format!("Daemon ({})", report.daemon.listen),
        report.daemon.status,
        &report.daemon.findings,
        &mut out,
    );
    out.push_str(if report.failed() {
        "Some checks failed.\n"
    } else {
        "All checks passed.\n"
    });
    out
}

/// Prints the report and reports whether every check passed.
pub fn print(report: &Report, json: bool) -> miette::Result<bool> {
    let text = if json {
        serde_json::to_string_pretty(report)
            .map_err(|error| miette::miette!("cannot encode the report: {error}"))?
            + "\n"
    } else {
        render(report)
    };
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|error| miette::miette!("cannot write the report: {error}"))?;
    Ok(!report.failed())
}
