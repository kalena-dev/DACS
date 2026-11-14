//! Shared server-rendered owner onboarding. All actions are explicit local POSTs.
use crate::{checkbox, form, hidden, input, json_details, page, pretty, textarea};
use leptos::prelude::*;
use serde_json::{Value, json};

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}
fn yes(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool) == Some(true)
}

pub fn render_onboarding(status: &Value, notice: Option<&str>, csrf: &str) -> String {
    let review = text(status, "review");
    let invitation = status.get("invitation").filter(|v| !v.is_null());
    let contract = status.get("contract").filter(|v| !v.is_null());
    let registered = yes(status, "credential_saved");
    let registration_started = yes(status, "registration_started");
    let blocker = status.get("blocker").and_then(Value::as_str).map(|message| view! {
        <aside class="warning" role="alert"><h2>"Current blocker"</h2><p>{message.to_owned()}</p></aside>
    });
    let review_card = invitation.map(|invitation| view! {
        <section><h2>"2. Review the named coordinator and requirements"</h2>
            <p><strong>"Inviting coordinator: "</strong>{text(invitation,"host_name")}</p>
            <p><strong>"Project: "</strong>{text(invitation,"project_id")}</p>
            {json_details("Exact invitation requirements and expiry", invitation)}
            <p>"The requirements manifest must contain requirements_version and required_packages: an array of reviewed catalogue IDs with optional versions. Unknown package IDs are blockers, never executable commands. Required declines disable affected capabilities. OMP and provider readiness are checked separately before worker admission."</p>
            {form("/ui/onboarding/plan",csrf,view! {
                {hidden("review",review.clone())}
                {textarea("Optional additional catalogue packages (JSON array of objects with id and optional version)","packages","[]",true)}
                {textarea("Declined package IDs (one per line)","declined","",false)}
            },"Detect installed tools and plan requirements")}
            <p><a href="/setup">"Browse the existing setup catalogue and optional tools"</a></p>
        </section>
    });
    let plan = status.get("plan").filter(|v| !v.is_null()).map(|plan| view! {
        <section><h2>"3. Review and apply the actual dependency plan"</h2>
            {json_details("Detected versions, required tools, exact commands and unresolved blockers",plan)}
            <p>"This uses the shared setup engine and its durable installation journal. No install occurs until this separate approval. A failed or interrupted job must be reviewed before resuming."</p>
            {form("/ui/onboarding/apply",csrf,view! {
                {hidden("review",review.clone())}
                {hidden("plan_sha256",text(plan,"sha256"))}
                {checkbox("I own this PC and approve this exact package plan and its commands.","owner_approved",true)}
                {checkbox("I approve administrator-level access for this local installation. Elevated execution is not sandboxed.","administrator_access_approved",true)}
                {checkbox("Resume an interrupted setup only after reviewing its journal and reconciling uncertain steps.","resume_interrupted",false)}
            },"Approve and apply with setup engine")}
        </section>
    });
    let setup_result = status
        .get("setup_result")
        .filter(|v| !v.is_null())
        .map(|result| json_details("Last setup engine result", result));
    let disclosure = contract.map(|contract| view! {
        <section><h2>"Network disclosure and infrastructure"</h2>
            <aside class="warning"><p>{text(contract,"admin_disclosure")}</p></aside>
            <p><strong>"Headscale login server: "</strong>{text(contract,"login_server")}</p>
            <p>"The network host label below names this PC on the private network. It is not the inviting coordinator name. These are separate identities and both must be reviewed."</p>
            {json_details("Current network contract, endpoints and disclosure revision",contract)}
        </section>
    });
    let registration = if !registration_started && contract.is_some() {
        invitation.map(|invitation| {
            let registration = json!({
                "name":"","physical_host_id":uuid::Uuid::new_v4().to_string(),
                "os":std::env::consts::OS,"architecture":std::env::consts::ARCH,
                "capabilities":[],"capacity":{"agent_slots":0,"cpu_millis":0,"memory_mb":0,"disk_mb":0},
                "omp_version":"","policy_revision":1,"owner_consented":true,
                "consent_host":text(invitation,"host_name")
            });
            view! { <section><h2>"4. Explicitly register this device"</h2>
                <p>"Set your device name, installed OMP version, stable physical-host identity, actual offered resources/capabilities and the coordinator's current policy revision. Zero capacity offers no work. Registration does not start the worker or mark provider login ready."</p>
                {form("/ui/onboarding/register",csrf,view! {
                    {hidden("review",review.clone())}
                    {textarea("Typed WorkerRegistration JSON (review every field)","registration",pretty(&registration),true)}
                    {input("This PC's network host label (lowercase single DNS label)","network_host","text","",true)}
                    {checkbox(&format!("I own this PC and explicitly accept administrator-level execution from the inviting coordinator {}. Elevated restrictions are not a sandbox.",text(invitation,"host_name")),"owner_approved",true)}
                    {checkbox("I accept the current network administrator-access disclosure above for this PC's named network host. Joining and switching networks still require separate approval.","network_approved",true)}
                },"Consent and register; do not join or start worker")}
            </section> }
        })
    } else {
        None
    };
    let pending = registration_started.then(|| view! {
        <section><h2>"5. Individual coordinator approval"</h2>
            <p>{if registered {
                format!("Device {} has a credential saved in private local state. Registration alone does not imply approval. The coordinator owner must individually approve this device; grant issuance and membership reconciliation remain separate gates.",text(status,"device_id"))
            } else {
                "A registration request was attempted but no credential is available locally. Its remote outcome may be uncertain. Ask the coordinator owner to reconcile/revoke that registration; this page will not blindly repeat it or overwrite credentials.".to_owned()
            }}</p>
            {registered.then(|| form("/ui/onboarding/refresh",csrf,view! { {hidden("review",review.clone())} },"Refresh actual approval / grant / membership"))}
            <p>{if yes(status,"grant_ready") { "A real device-specific grant was available at the last refresh. It will be fetched again and checked before joining." } else { "No current usable grant has been confirmed. Pending approval is not network membership." }}</p>
            {status.get("network_state").filter(|v|!v.is_null()).map(|value|json_details("Coordinator's actual safe per-device network state",value))}
        </section>
    });
    let retry_consent = (registered && !yes(status,"consent_sent") && contract.is_some()).then(|| view! {
        <section><h2>"Send the saved device's network consent"</h2>
            <p>"Use this if enrollment succeeded but sending consent was interrupted. This reuses the private credential, not a new registration."</p>
            {form("/ui/onboarding/consent",csrf,view! {
                {hidden("review",review.clone())}
                {input("Previously accepted network host label","network_host","text",status.get("consent").map(|c|text(c,"accepted_host")).unwrap_or_default(),true)}
                {checkbox("I explicitly accept the current administrator-access network disclosure for this network host.","network_approved",true)}
            },"Send explicit network consent")}
        </section>
    });
    let assess = (registered && contract.is_some()).then(|| view! {
        <section><h2>"6. Assess the real local Tailscale network"</h2>
            <p>"This reads the installed client and daemon state without changing them. A missing client must be installed separately using reviewed setup or the platform owner's approved procedure; an unavailable daemon must be started locally."</p>
            {form("/ui/onboarding/assess",csrf,view! { {hidden("review",review.clone())} },"Assess current local network (no changes)")}
        </section>
    });
    let assessment = status.get("assessment").filter(|v|!v.is_null()).map(|assessment| {
        let switch_required = yes(assessment,"requires_switch_consent");
        let switch = switch_required.then(|| view! {
            <aside class="warning"><p>{text(assessment,"disclosure")}</p>
                <p><strong>"Network to disconnect: "</strong>{text(assessment,"previous_control_url")}</p>
            </aside>
        });
        let client_state = assessment.get("client").map(|c|text(c,"state")).unwrap_or_default();
        let eligible = registered && !yes(status,"join_started") && !yes(assessment,"already_dacs")
            && matches!(client_state.as_str(),"logged_out" | "logged_in");
        view! { <section><h2>"7. Separate network join and switch consent"</h2>
            {json_details("Actual local assessment",assessment)}{switch}
            {eligible.then(|| form("/ui/onboarding/join",csrf,view! {
                {hidden("review",review.clone())}
                {hidden("assessment_id",text(status,"assessment_id"))}
                {hidden("previous_control_url",text(assessment,"previous_control_url"))}
                {checkbox("I explicitly approve joining this device to the reviewed Headscale login server using its individually approved grant.","join_approved",true)}
                {switch_required.then(||checkbox("Separately, I approve disconnecting the exact existing Tailscale network shown above until I switch back.","switch_approved",true))}
                {checkbox("Accept this network's MagicDNS configuration.","accept_dns",false)}
            },"Join explicitly and refresh real membership"))}
            {(!eligible).then(||view! { <p class="warning">"Join is not currently actionable: review whether Tailscale is missing/unavailable, this PC is already on DACS, or a previous join needs reconciliation. Refresh actual membership; do not treat a local profile as coordinator approval."</p> })}
        </section> }
    });
    let joined = status.get("join_result").filter(|v|!v.is_null()).map(|result| view! {
        <section><h2>"Local network join result"</h2>{json_details("Non-secret LocalJoinResult",result)}
            <p>"The local client confirmed this result. Coordinator membership must also be refreshed and confirmed. Neither is evidence of provider readiness or a running worker."</p>
        </section>
    });
    let worker_setup = registered.then(|| form("/ui/onboarding/configure_worker", csrf, view! {
        {hidden("review", &review)}
        {input("Absolute path to the installed OMP executable", "omp_executable", "text", "", true)}
        {input("Owner's OMP profile (optional)", "omp_profile", "text", "", false)}
        {checkbox("Store the enrolled credential and worker configuration privately on this PC", "worker_config_approved", true)}
    }, "Prepare manual worker launch"));
    let worker_controls = status.get("worker_config_file").and_then(Value::as_str).map(|path| view! {
        <p>"Run this command manually after provider login and coordinator approval. Ctrl-C drains admitted work; emergency cancellation is separate."</p>
        <pre>{format!("dacs worker run --config \"{path}\"")}</pre>
        {json_details("Observed local worker status", &status["worker_status"])}
        {form("/ui/onboarding/worker_control", csrf, view! {
            {hidden("review", &review)}
            <label>"Local control"<select name="action">
                <option value="pause_desktop">"Pause desktop access"</option>
                <option value="resume_desktop">"Resume desktop access"</option>
                <option value="drain">"Drain worker; finish admitted work"</option>
            </select></label>
        }, "Request local control")}
        <details><summary>"Emergency cancellation"</summary>
            {form("/ui/onboarding/worker_control", csrf, view! {
                {hidden("review", &review)}{hidden("action", "emergency_cancel")}
                {checkbox("Cancel admitted work now; external effects may require reconciliation", "emergency_approved", true)}
            }, "Emergency cancel")}
        </details>
        <p>"Control requests are durable. Refresh to inspect what the running worker has actually observed; a request is not proof of termination."</p>
    });
    page(
        "Local owner onboarding",
        notice,
        view! {
            <div class="onboarding-layout">
            <h1>"Enroll this PC with a named coordinator"</h1>
            <p>"Authenticated local owner only. Opening this page or any invitation link never enrolls, installs, changes a network, logs in to a provider or launches a worker. Each operation runs in the existing local Jobs journal; follow its job link, then return here to continue."</p>
            <div class="actions"><a class="button" href="/onboarding">"Refresh saved onboarding status"</a><a class="button" href="/setup">"Local setup and jobs"</a></div>
            {blocker}
            <section><details open=invitation.is_none()><summary>"1. Inspect a remote invitation"</summary>
                <p>"Use the coordinator's public bootstrap HTTPS origin (normally port 7444). Paste the invitation token only into this authenticated local POST, never a local query string. Optional custom CA trust remains local; TLS verification is never disabled."</p>
                {form("/ui/onboarding/inspect",csrf,view! {
                    {input("Coordinator bootstrap origin","endpoint","url",text(status,"endpoint"),true)}
                    {input("Invitation token","invitation_token","password","",true)}
                    {input("Trusted CA PEM file on this PC (optional)","ca_file","text","",false)}
                },"Inspect invitation and current network disclosure")}
            </details></section>
            {review_card}{plan}{setup_result}{disclosure}{registration}{pending}{retry_consent}{assess}{assessment}{joined}
            <section><h2>"Manual readiness and worker launch"</h2>
                <p>"Complete provider login manually through the installed pinned OMP on this PC. Never paste provider credentials here. Review and launch the worker manually only after project approval, required tool compatibility and confirmed network membership. Onboarding never starts OMP login or the worker."</p>
                <p>"The device credential remains in the owner's private DACS state directory under onboarding/owner.json. Do not publish or attach this file to job reports. The LocalJoinResult above contains only non-secret network handoff information."</p>
                {worker_setup}{worker_controls}
            </section>
            </div>
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn joining_uses_separate_switch_consent_and_only_post_forms() {
        let status = json!({"review":"review","credential_saved":true,"registration_started":true,
            "assessment_id":"assessment","assessment":{"requires_switch_consent":true,"previous_control_url":"https://old.example.test","client":{"state":"logged_in"}}});
        let html = render_onboarding(&status, None, "csrf-value");
        assert!(html.contains("action=\"/ui/onboarding/join\""));
        assert!(html.contains("name=\"join_approved\""));
        assert!(html.contains("name=\"switch_approved\""));
        assert!(html.contains("name=\"csrf\""));
        assert!(!html.contains("method=\"get\""));
    }
}
