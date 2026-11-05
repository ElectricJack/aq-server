#[path = "../tests/scoped_presence_common.rs"]
mod scoped_presence_common;
use aq_admission::scoped_presence::*;
fn main() {
    let kind = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "report-fixture".into());
    let value = match kind.as_str() {
        "inventory-schema" => inventory_schema(),
        "report-schema" => report_schema(),
        "receipt-schema" => receipt_schema(),
        "inventory-receipt-schema" => inventory_receipt_schema(),
        "qualification-schema" => qualification_schema(),
        "inventory-fixture" => serde_json::to_value(scoped_presence_common::inventory()).unwrap(),
        "qualification-fixture" => {
            serde_json::to_value(scoped_presence_common::qualification()).unwrap()
        }
        "private-record-fixture" => serde_json::to_value(scoped_presence_common::record()).unwrap(),
        "report-fixture" => serde_json::to_value(scoped_presence_common::report()).unwrap(),
        _ => panic!("unknown scoped-presence export"),
    };
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}
