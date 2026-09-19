//! Run every panel of every dashboard, over the real control socket.
//!
//! It exists so that a saved dashboard can be checked through the running loop
//! rather than only in a test. A panel holds an encoded `QueryRequest`, and the
//! thing worth proving is that the head answers it — a starter dashboard whose
//! panels the head refuses would be a screen that reads "this installation
//! cannot answer that" on first run.
//!
//!     ./tools.sh dev up
//!     cargo run -p tallyowl-driver-rust --example run_dashboard
use tallyowl_control_api::codec::{
    decode_query_response, decode_saved_analysis_list, decode_saved_dashboard_list,
    decode_service_error, encode_saved_request,
};
use tallyowl_control_api::types::{ListRequest, SavedRequest};
use tallyowl_rpc::SERVICE_ERROR_VARIANT;

fn main() {
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
            tallyowl_control_api::codec::encode_list_request(&ListRequest {
                cursor: None,
                limit: None,
            }),
        )
        .expect("the head answers");
    let projects =
        tallyowl_control_api::codec::decode_project_list(&projects.payload).expect("a list");

    let mut panels = 0;
    let mut answered = 0;
    for project in &projects.projects {
        let ask = |op: &str| {
            client
                .call(
                    "TallyOwlControl",
                    op,
                    encode_saved_request(&SavedRequest {
                        project_id: project.project_id.clone(),
                        id: None,
                        cursor: None,
                        limit: None,
                    }),
                )
                .expect("the head answers")
        };
        let dashboards =
            decode_saved_dashboard_list(&ask("list-dashboards").payload).expect("a dashboard list");
        let analyses =
            decode_saved_analysis_list(&ask("list-analyses").payload).expect("an analysis list");

        for dashboard in &dashboards.dashboards {
            println!("\n{} — {}", project.name, dashboard.name);
            for panel in &dashboard.panels {
                panels += 1;
                let Some(analysis) = analyses
                    .analyses
                    .iter()
                    .find(|a| a.analysis_id == panel.analysis_id)
                else {
                    println!("  {}: no analysis behind this panel", panel.analysis_id);
                    continue;
                };

                // The saved request, sent exactly as it was stored.
                let response = client
                    .call("TallyOwlControl", "run-query", analysis.request.clone())
                    .expect("the head answers");
                if response.variant.as_deref() == Some(SERVICE_ERROR_VARIANT) {
                    let error = decode_service_error(&response.payload).expect("an error");
                    println!("  {}: REFUSED — {}", analysis.name, error.message);
                    continue;
                }
                let result = decode_query_response(&response.payload).expect("a result");
                answered += 1;
                println!(
                    "  {}: {} rows, columns {:?}",
                    analysis.name,
                    result.rows.len(),
                    result.columns
                );
            }
        }
    }

    println!("\n{answered} of {panels} panels answered.");
    if answered != panels {
        std::process::exit(1);
    }
}
