//! Set one project's collection policy over the real control socket.
//!
//! It exists so that policy distribution can be checked through the running
//! loop rather than only in a test. `./tools.sh dev up`, then this, then read
//! `run/collector.log`: the collector fetches, applies, and names the version.
//!
//!     cargo run -p tallyowl-driver-rust --example put_policy -- <blocked-name>
use tallyowl_control_api::codec::{
    decode_compiled_policy, decode_project_list, encode_list_request, encode_policy_document,
};
use tallyowl_control_api::types::{ListRequest, PolicyDocument, PolicyScope};

fn main() {
    let blocked = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "debug-ping".into());
    let session = std::fs::read_to_string("data/operator.session")
        .expect("`./tools.sh dev up` writes data/operator.session")
        .trim()
        .to_string();
    let client =
        tallyowl_rpc::Client::new("127.0.0.1:5110", 16 * 1024 * 1024).with_credential(session);

    let projects = client
        .call(
            "TallyOwlControl",
            "list-projects",
            encode_list_request(&ListRequest {
                cursor: None,
                limit: None,
            }),
        )
        .expect("the head answers");
    let projects = decode_project_list(&projects.payload).expect("a project list");
    let project = projects.projects.first().expect("one project exists");
    let project_id: String = project
        .project_id
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    println!("project {} ({project_id})", project.name);

    let response = client
        .call(
            "TallyOwlControl",
            "put-policy",
            encode_policy_document(&PolicyDocument {
                scope: PolicyScope::Project,
                scope_id: Some(project_id),
                enabled_kinds: None,
                head_sample_rate: None,
                session_max_lifetime_ms: None,
                max_event_bytes: None,
                max_properties: None,
                blocked_event_names: Some(vec![blocked]),
                blocked_property_keys: None,
                redact_property_keys: None,
                campaign_linking: None,
                attribution_needs_consent: None,
                kill_switch: None,
            }),
        )
        .expect("the head answers");
    let compiled = decode_compiled_policy(&response.payload).expect("a compiled policy");
    println!("policy version {}", compiled.policy_version);
    println!("blocked {:?}", compiled.blocked_event_names);
}
