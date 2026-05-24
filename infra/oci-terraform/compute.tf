resource "oci_core_instance" "travsr_mcp_server" {
  compartment_id      = var.compartment_ocid
  availability_domain = var.availability_domain
  display_name        = var.instance_display_name

  # VM.Standard.A1.Flex is the OCI Always Free ARM64 shape.
  # Shape is hardcoded — do not parameterize; changing shape type requires
  # instance replacement and risks exceeding Always Free limits.
  shape = "VM.Standard.A1.Flex"

  shape_config {
    ocpus         = 2
    memory_in_gbs = 12
  }

  source_details {
    source_type = "image"
    source_id   = var.image_ocid
  }

  create_vnic_details {
    subnet_id        = oci_core_subnet.travsr_public_subnet.id
    assign_public_ip = true
    display_name     = "travsr-mcp-server-vnic"
  }

  metadata = {
    ssh_authorized_keys = var.ssh_public_key
  }
}

resource "oci_core_volume" "travsr_data" {
  compartment_id      = var.compartment_ocid
  availability_domain = var.availability_domain
  display_name        = "travsr-data-volume"
  size_in_gbs         = var.block_volume_size_gb

  # vpus_per_gb = 0 → Lower Cost performance tier.
  # This is the only tier compatible with the OCI Always Free block volume quota.
  # Do NOT increase without verifying Always Free entitlement.
  vpus_per_gb = 0
}

resource "oci_core_volume_attachment" "travsr_data_attachment" {
  attachment_type = "paravirtualized"
  instance_id     = oci_core_instance.travsr_mcp_server.id
  volume_id       = oci_core_volume.travsr_data.id
}
