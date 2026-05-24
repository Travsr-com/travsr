resource "oci_artifacts_container_repository" "travsr_mcp" {
  compartment_id = var.compartment_ocid
  display_name   = var.ocir_repo_name

  # Private repository — images require authenticated docker login.
  # See infra/scripts/setup-instance.sh Step 7 for docker login instructions.
  is_public = false

  # Mutable tags allow re-pushing the same tag (e.g., :latest) during the
  # Cloud Preview phase. Set is_immutable = true before GA to enforce tag
  # immutability for supply-chain integrity.
  is_immutable = false
}
