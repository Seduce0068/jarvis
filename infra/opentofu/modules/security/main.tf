# Security Module

resource "oci_core_network_security_group" "nsg" {
  compartment_id = var.compartment_id
  vcn_id         = var.vcn_id
  display_name   = "jarvis-nsg"

  freeform_tags = var.tags
}

# SSH access (restricted to allowed CIDRs or Tailscale only)
resource "oci_core_network_security_group_security_rule" "ssh" {
  count = length(var.allowed_ssh_cidrs) > 0 ? 1 : 0

  network_security_group_id = oci_core_network_security_group.nsg.id
  direction                 = "INGRESS"
  protocol                  = "6" # TCP
  source                    = join(",", var.allowed_ssh_cidrs)
  source_type               = "CIDR_BLOCK"
  stateless                 = false
  description               = "SSH access"

  tcp_options {
    destination_port_range {
      min = 22
      max = 22
    }
  }
}

# Tailscale (UDP 41641 + TCP 443 for DERP)
resource "oci_core_network_security_group_security_rule" "tailscale_udp" {
  network_security_group_id = oci_core_network_security_group.nsg.id
  direction                 = "INGRESS"
  protocol                  = "17" # UDP
  source                    = "0.0.0.0/0"
  source_type               = "CIDR_BLOCK"
  stateless                 = false
  description               = "Tailscale UDP"

  udp_options {
    destination_port_range {
      min = 41641
      max = 41641
    }
  }
}

resource "oci_core_network_security_group_security_rule" "tailscale_tcp" {
  network_security_group_id = oci_core_network_security_group.nsg.id
  direction                 = "INGRESS"
  protocol                  = "6" # TCP
  source                    = "0.0.0.0/0"
  source_type               = "CIDR_BLOCK"
  stateless                 = false
  description               = "Tailscale TCP (DERP)"

  tcp_options {
    destination_port_range {
      min = 443
      max = 443
    }
  }
}

# code-server (internal only - accessed via Tailscale)
resource "oci_core_network_security_group_security_rule" "code_server" {
  count = length(var.allowed_web_cidrs) > 0 ? 1 : 0

  network_security_group_id = oci_core_network_security_group.nsg.id
  direction                 = "INGRESS"
  protocol                  = "6"
  source                    = join(",", var.allowed_web_cidrs)
  source_type               = "CIDR_BLOCK"
  stateless                 = false
  description               = "code-server (restricted)"

  tcp_options {
    destination_port_range {
      min = 8080
      max = 8080
    }
  }
}

# OpenCode Web (internal only)
resource "oci_core_network_security_group_security_rule" "opencode_web" {
  count = length(var.allowed_web_cidrs) > 0 ? 1 : 0

  network_security_group_id = oci_core_network_security_group.nsg.id
  direction                 = "INGRESS"
  protocol                  = "6"
  source                    = join(",", var.allowed_web_cidrs)
  source_type               = "CIDR_BLOCK"
  stateless                 = false
  description               = "OpenCode Web (restricted)"

  tcp_options {
    destination_port_range {
      min = 49374
      max = 49374
    }
  }
}

# Internal service communication (JARVIS services)
resource "oci_core_network_security_group_security_rule" "internal" {
  network_security_group_id = oci_core_network_security_group.nsg.id
  direction                 = "INGRESS"
  protocol                  = "6"
  source                    = var.subnet_cidr
  source_type               = "CIDR_BLOCK"
  stateless                 = false
  description               = "Internal JARVIS services"

  tcp_options {
    destination_port_range {
      min = 50051
      max = 50054
    }
  }
}

# HTTP health/metrics (internal)
resource "oci_core_network_security_group_security_rule" "health" {
  network_security_group_id = oci_core_network_security_group.nsg.id
  direction                 = "INGRESS"
  protocol                  = "6"
  source                    = var.subnet_cidr
  source_type               = "CIDR_BLOCK"
  stateless                 = false
  description               = "Health/Metrics endpoints"

  tcp_options {
    destination_port_range {
      min = 8080
      max = 8084
    }
  }
}

# Egress: allow all for package installs, API calls
resource "oci_core_network_security_group_security_rule" "egress_all" {
  network_security_group_id = oci_core_network_security_group.nsg.id
  direction                 = "EGRESS"
  protocol                  = "all"
  destination               = "0.0.0.0/0"
  destination_type          = "CIDR_BLOCK"
  stateless                 = false
  description               = "Allow all egress"
}

output "nsg_id" {
  value = oci_core_network_security_group.nsg.id
}