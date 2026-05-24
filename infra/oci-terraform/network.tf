resource "oci_core_vcn" "travsr_vcn" {
  compartment_id = var.compartment_ocid
  cidr_block     = var.vcn_cidr
  display_name   = "travsr-vcn"
  dns_label      = "travsr"
}

resource "oci_core_internet_gateway" "travsr_igw" {
  compartment_id = var.compartment_ocid
  vcn_id         = oci_core_vcn.travsr_vcn.id
  display_name   = "travsr-igw"
  enabled        = true
}

resource "oci_core_route_table" "travsr_rt" {
  compartment_id = var.compartment_ocid
  vcn_id         = oci_core_vcn.travsr_vcn.id
  display_name   = "travsr-rt"

  route_rules {
    destination       = "0.0.0.0/0"
    destination_type  = "CIDR_BLOCK"
    network_entity_id = oci_core_internet_gateway.travsr_igw.id
  }
}

resource "oci_core_subnet" "travsr_public_subnet" {
  compartment_id             = var.compartment_ocid
  vcn_id                     = oci_core_vcn.travsr_vcn.id
  cidr_block                 = var.subnet_cidr
  display_name               = "travsr-public-subnet"
  dns_label                  = "pub"
  prohibit_public_ip_on_vnic = false
  route_table_id             = oci_core_route_table.travsr_rt.id
  security_list_ids          = [oci_core_security_list.travsr_sl.id]
}
