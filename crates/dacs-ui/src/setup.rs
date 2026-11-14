use crate::{checkbox, empty, form, hidden, input, json_details, page, pretty, textarea};
use leptos::prelude::*;
use serde_json::Value;

const CATEGORIES: [(&str, &str); 8] = [
    ("rust_toolchains", "Rust toolchains"),
    ("rust_components", "rustfmt / Clippy"),
    ("rust_web", "Rust web tooling"),
    ("git", "Git"),
    ("debuggers", "Debuggers"),
    ("editors", "Editors"),
    ("browsers", "Browsers"),
    ("databases", "Databases"),
];

/// `status` can be an engine result, a job result envelope or an object containing
/// `plan`. Approval is offered only for an actual digest-bearing plan with steps.
/// The server must retain job results privately and never put plan/token JSON in URLs.
pub fn render_setup(
    catalogue: &Value,
    status: Option<&Value>,
    notice: Option<&str>,
    csrf: &str,
) -> String {
    let packages = catalogue.get("packages").and_then(Value::as_array);
    let category_cards = CATEGORIES.iter().map(|(id, label)| {
        let entries: Vec<_> = packages.into_iter().flatten().filter(|p| p.get("category").and_then(Value::as_str) == Some(*id)).collect();
        view! { <details><summary>{*label}</summary><fieldset><legend>{*label}</legend>
            {empty(entries.is_empty(), "No reviewed entries supplied for this category. Unavailable; do not substitute an unreviewed command.")}
            {entries.into_iter().map(|p| package_card(p)).collect_view()}
        </fieldset></details> }
    }).collect_view();
    let plan = status.and_then(extract_plan);
    let plan_review = plan.map(|plan| {
        let steps = plan.get("steps").and_then(Value::as_array);
        let digest = plan.get("sha256").and_then(Value::as_str).unwrap_or_default();
        view! { <section id="review"><h2>"Review exact dependency plan"</h2>
            <p>"Review every command, prerequisite, required version, unresolved finding and disabled capability. Existing tools are reused only when detection supports it. Updating requirements never implies approval."</p>
            <dl><dt>"Reviewed plan SHA-256"</dt><dd><code>{digest.to_owned()}</code></dd><dt>"Platform / architecture"</dt><dd>{format!("{} / {}", text(plan, "platform"), text(plan, "arch"))}</dd></dl>
            <div class="table-scroll"><table><caption>"Detected and planned package states"</caption><thead><tr><th scope="col">"Package"</th><th scope="col">"Required version"</th><th scope="col">"Reported state"</th><th scope="col">"Commands and prerequisites"</th></tr></thead><tbody>
                {steps.into_iter().flatten().map(|step| view! { <tr><th scope="row">{text(step, "package_id")}</th><td>{text(step, "required_version")}</td><td>{text(step, "state")}</td><td>{json_details("Exact step", step)}</td></tr> }).collect_view()}
            </tbody></table></div>
            {json_details("Complete plan, unresolved requirements and disabled capabilities", plan)}
            <aside class="warning"><p>"Package managers may request elevation. Arbitrary host scripts are not catalogue-managed and are not granted generic rollback guarantees. Local approval below does not itself enroll a network, approve a device or launch a worker."</p></aside>
            {form("/ui/setup/apply", csrf, view! {
                {hidden("plan", pretty(plan))}{hidden("plan_sha256", digest)}
                {checkbox("I am this device's owner and approve this exact plan and its commands.", "owner_approved", true)}
                {checkbox("I approve the disclosed administrator-level access for this reviewed local operation and named invitation host, if any. Elevated controls are not a sandbox.", "administrator_access_approved", true)}
                {checkbox("Resume an interrupted operation only after reviewing its journal and reconciling uncertain steps.", "resume_interrupted", false)}
            }, "Approve and apply locally")}
        </section> }
    });
    let result = status.map(|result| view! {
        <section id="operation"><h2>"Actual operation status"</h2>{json_details("Detection, scan, job or operation result", result)}
            {result.get("id").or_else(|| result.get("job_id")).and_then(Value::as_str).filter(|id| safe_id(id)).map(|id| view! { <a class="button" href=format!("/setup/jobs/{id}")>"Refresh this operation"</a> })}
            <p>"Queued or running is not completed. Pending external enrollment is not network membership or worker execution. Review failed, interrupted and unresolved records before retrying."</p>
        </section>
    });
    page(
        "Local setup",
        notice,
        view! {
            <div class="setup-layout">
            <h1>"Local owner setup"</h1>
            <p>"This page must be served by an authenticated loopback-only setup listener. Public installation and invitation viewing confer no access. Choose optional tools; the catalogue is not an instruction to install every category."</p>
            <aside class="warning"><strong>"Consent boundary"</strong><p>"The local owner approves installation and named-host administrator access. Host device approval, network membership, project readiness and manual OMP provider login remain separate gates. Never paste provider secrets into this page."</p></aside>
            {result}{plan_review}
            {empty(plan.is_none(), "No reviewable plan available. Detection/planning or job completion is required before local approval.")}
            <section><h2>"1. Detect this device"</h2><p>"Read installed versions and package-manager availability before planning. A missing report means unknown, not absent or incompatible."</p>
                {form("/ui/setup/detect", csrf, (), "Detect installed tools")}
                {json_details("Catalogue platform and supported recipes", catalogue)}
            </section>
            <section><h2>"2. Existing-project requirements"</h2><p>"Scan repository manifests without executing project files. Optional OMP analysis is read-only and requires local manual login; unavailable authentication is an error, not a fabricated scan. Review evidence and unresolved constraints before mapping the result into a plan."</p>
                {form("/ui/setup/scan", csrf, view! {
                    {input("Local repository path", "repo", "text", "", true)}{checkbox("Use locally authenticated OMP for read-only requirements analysis", "use_ai", false)}
                }, "Scan existing project")}
            </section>
            <section><h2>"3. Select a new-project preset or reviewed packages"</h2><p>"Existing project scans and invitation manifests are review inputs. Unsupported versions and sources stay unresolved. Project crate dependencies remain managed by Cargo, not global setup."</p>
                {form("/ui/setup/plan", csrf, view! {
                    <div class="grid"><label><span>"Operation"</span><select name="operation"><option value="install">"Install missing requirements"</option><option value="repair">"Repair selected tools"</option><option value="update">"Owner-requested dependency update"</option></select></label>
                    <label><span>"New-project preset"</span><select name="preset"><option value="">"No preset — choose packages"</option><option value="rust-cli">"Rust CLI"</option><option value="rust-web">"Rust web"</option></select></label></div>
                    <div class="catalogue">{category_cards}</div>
                    {textarea("Declined package IDs (one per line; required declines disable affected capabilities)", "declined", "", false)}
                    {textarea("Invitation requirements JSON, if enrolling (coordinator_name, invitation_token, requirements_version, required_packages)", "invitation", "", false)}
                    <p>"Invitation secrets belong only in this authenticated local POST, never a URL. The resulting plan must name the host before local approval. No arbitrary scripts or unreviewed sources are accepted."</p>
                }, "Detect and create reviewable plan")}
            </section>
            <section><details class="advanced"><summary>"4. Signed application updates"</summary><p>"Updates use the locally configured trusted Ed25519 public key and actual current app version, never a key supplied by a coordinator or manifest. A missing trusted key blocks verification. Staging does not mean installation; review the result before applying. Dependency updates above remain owner-controlled."</p>
                <details><summary>"Verify a release signature"</summary>{form("/ui/setup/update/verify", csrf, textarea("Signed release envelope JSON", "manifest", "", true), "Verify signature and compatibility")}</details>
                <details><summary>"Download and stage a verified release"</summary>{form("/ui/setup/update/stage", csrf, textarea("Signed release envelope JSON to download and stage", "manifest", "", true), "Verify and stage update")}</details>
                <details><summary>"Apply staged update using local helper"</summary><p>"Stop the target application and reconcile pending work first. Apply from the separate helper, especially on Windows where a running executable is locked. Failed replacement must preserve the previous executable and recovery evidence."</p>
                    {form("/ui/setup/update/apply", csrf, input("Absolute local target executable path", "target", "text", "", true), "Apply verified staged update")}
                </details>
                <details><summary>"Roll back a recorded application update"</summary><p>"Use the retained local update journal and backup. Stop the target application first. Rollback is available only when the engine can verify the recorded previous executable; it does not undo project or dependency changes."</p>
                    {form("/ui/setup/update/rollback", csrf, input("Absolute local target executable path", "target", "text", "", true), "Restore verified previous executable")}
                </details>
            </details></section>
            </div>
        },
    )
}

fn text(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => "Not specified / unknown".into(),
        Some(other) => other.to_string(),
    }
}

fn extract_plan(value: &Value) -> Option<&Value> {
    if value
        .get("sha256")
        .and_then(Value::as_str)
        .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        && value.get("steps").is_some_and(Value::is_array)
        && value.get("request").is_some()
    {
        return Some(value);
    }
    value
        .get("plan")
        .or_else(|| value.get("result"))
        .and_then(extract_plan)
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn package_card(package: &Value) -> AnyView {
    let id = text(package, "id");
    let label = text(package, "label");
    view! { <div class="package"><label class="check"><input type="checkbox" name=format!("package.{id}") value="true"/><span>{label}</span></label>
        <p><code>{id.clone()}</code></p><label><span>"Required version (blank means catalogue default)"</span><input type="text" name=format!("version.{id}")/></label>
        {json_details("Supported platforms, prerequisites and reviewed recipes", package)}
    </div> }.into_any()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn all_categories_remain_visible_without_detection() {
        let html = render_setup(&json!({}), None, None, "csrf");
        for (_, label) in CATEGORIES {
            assert!(html.contains(label), "missing {label}");
        }
        assert!(!html.contains("action=\"/ui/setup/apply\""));
        assert!(html.contains("No reviewable plan available"));
    }

    #[test]
    fn plan_approval_binds_the_reviewed_digest() {
        let digest = "a".repeat(64);
        let plan =
            json!({"sha256":digest,"request":{},"steps":[],"platform":"windows","arch":"x86_64"});
        let html = render_setup(&json!({}), Some(&plan), None, "csrf");
        assert!(html.contains("name=\"plan_sha256\""));
        assert!(html.contains("name=\"owner_approved\""));
        assert!(html.contains("name=\"administrator_access_approved\""));
        assert!(html.contains("action=\"/ui/setup/apply\""));
    }

    #[test]
    fn incomplete_plan_and_job_path_injection_do_not_enable_actions() {
        let status = json!({"sha256":"bad","steps":[],"id":"../../session?token=secret"});
        let html = render_setup(&json!({}), Some(&status), None, "csrf");
        assert!(!html.contains("action=\"/ui/setup/apply\""));
        assert!(!html.contains("href=\"/setup/jobs/"));
    }
}
