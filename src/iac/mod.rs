//! IaC adapters. Each one turns a tool-specific input into `model::Input`.

pub mod terraform_hcl;
pub mod terraform_plan;
