// Cluster configuration for testnet deployments.
// As of 2022Q4, we're reusing a single cluster to host
// multiple environments, e.g. "testnet" and "testnet-preview".
// We may migrate to multiple clusters in the future.
module "gcp_terraform_state_testnet" {
  source = "../../terraform/modules/gcp/terraform_state/chain"

  chain_name          = "penumbra"
  labels              = {}
  location            = "US"
  network_environment = "testnet"
}

module "gke_testnet" {
  source = "../../terraform/modules/node/v1"

  project_id    = "penumbra-sl-testnet"
  cluster_name  = "testnet"
  region        = "us-central1"
  cluster_zones = ["us-central1-a", "us-central1-b"]
  machine_type  = "n2d-standard-4"
}
