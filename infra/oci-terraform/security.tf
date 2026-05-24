resource "oci_core_security_list" "travsr_sl" {
  compartment_id = var.compartment_ocid
  vcn_id         = oci_core_vcn.travsr_vcn.id
  display_name   = "travsr-sl"

  # Egress: allow all outbound traffic
  egress_security_rules {
    destination = "0.0.0.0/0"
    protocol    = "all"
    stateless   = false
  }

  # Ingress: SSH (22) — open to internet for operator access
  ingress_security_rules {
    protocol  = "6" # TCP
    source    = "0.0.0.0/0"
    stateless = false

    tcp_options {
      min = 22
      max = 22
    }
  }

  # Ingress: HTTP (80) — open to internet for HTTPS redirect
  ingress_security_rules {
    protocol  = "6" # TCP
    source    = "0.0.0.0/0"
    stateless = false

    tcp_options {
      min = 80
      max = 80
    }
  }

  # Ingress: HTTPS (443) — open to internet for MCP SSE traffic
  ingress_security_rules {
    protocol  = "6" # TCP
    source    = "0.0.0.0/0"
    stateless = false

    tcp_options {
      min = 443
      max = 443
    }
  }

  # Ingress: axum internal port (3000) — VCN-internal ONLY, never public.
  # Nginx reverse-proxies localhost:3000; this rule covers intra-VCN access only.
  # Port 3000 is intentionally absent from UFW rules in setup-instance.sh.
  ingress_security_rules {
    protocol  = "6" # TCP
    source    = var.vcn_cidr
    stateless = false

    tcp_options {
      min = 3000
      max = 3000
    }
  }
}
