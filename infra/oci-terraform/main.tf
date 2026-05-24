terraform {
  required_version = ">= 1.6"

  required_providers {
    oci = {
      source  = "hashicorp/oci"
      version = "~> 6.0"
    }
  }

  # Local backend — no remote state. terraform.tfstate is gitignored.
  # Operators must store tfstate securely (e.g., OCI Object Storage) before
  # adding team members. See infra/oci-terraform/terraform.tfvars.example.
  backend "local" {}
}

provider "oci" {
  tenancy_ocid     = var.tenancy_ocid
  user_ocid        = var.user_ocid
  fingerprint      = var.fingerprint
  private_key_path = var.private_key_path
  region           = var.region
}
