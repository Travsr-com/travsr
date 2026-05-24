output "instance_public_ip" {
  description = "Public IP address of the travsr-mcp-server instance. Point mcp.travsr.com DNS A record here before running Certbot."
  value       = oci_core_instance.travsr_mcp_server.public_ip
}

output "ocir_url" {
  description = "Full OCIR URL for the travsr-mcp repository. Use this as the image prefix when tagging and pushing Docker images."
  value       = "${var.region}.ocir.io/${var.ocir_namespace}/${var.ocir_repo_name}"
}

output "block_volume_id" {
  description = "OCID of the travsr-data block volume. Use this if you need to manually attach, detach, or back up the volume via OCI Console or CLI."
  value       = oci_core_volume.travsr_data.id
}
