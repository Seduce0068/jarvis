# Network Module

resource "oci_core_virtual_network" "vcn" {
  compartment_id = var.compartment_id
  cidr_block     = var.vcn_cidr
  dns_label      = "jarvisvcn"
  display_name   = "jarvis-vcn"

  freeform_tags = var.tags
}

resource "oci_core_subnet" "subnet" {
  compartment_id        = var.compartment_id
  vcn_id                = oci_core_virtual_network.vcn.id
  cidr_block            = var.subnet_cidr
  dns_label             = "jarvissubnet"
  display_name          = "jarvis-subnet"
  availability_domain   = var.availability_domain
  prohibit_public_ip_on_vnic = false

  freeform_tags = var.tags
}

resource "oci_core_internet_gateway" "igw" {
  compartment_id = var.compartment_id
  vcn_id         = oci_core_virtual_network.vcn.id
  display_name   = "jarvis-igw"
  is_enabled     = true

  freeform_tags = var.tags
}

resource "oci_core_nat_gateway" "nat" {
  compartment_id = var.compartment_id
  vcn_id         = oci_core_virtual_network.vcn.id
  display_name   = "jarvis-nat"
  block_traffic  = false

  freeform_tags = var.tags
}

resource "oci_core_route_table" "public" {
  compartment_id = var.compartment_id
  vcn_id         = oci_core_virtual_network.vcn.id
  display_name   = "jarvis-rt-public"

  route_rules {
    network_entity_id = oci_core_internet_gateway.igw.id
    destination       = "0.0.0.0/0"
    destination_type  = "CIDR_BLOCK"
  }

  freeform_tags = var.tags
}

resource "oci_core_route_table" "private" {
  compartment_id = var.compartment_id
  vcn_id         = oci_core_virtual_network.vcn.id
  display_name   = "jarvis-rt-private"

  route_rules {
    network_entity_id = oci_core_nat_gateway.nat.id
    destination       = "0.0.0.0/0"
    destination_type  = "CIDR_BLOCK"
  }

  freeform_tags = var.tags
}

resource "oci_core_default_route_table" "default" {
  manage_default_resource_id = oci_core_virtual_network.vcn.default_route_table_id

  route_rules {
    network_entity_id = oci_core_nat_gateway.nat.id
    destination       = "0.0.0.0/0"
    destination_type  = "CIDR_BLOCK"
  }
}

output "vcn_id" {
  value = oci_core_virtual_network.vcn.id
}

output "subnet_id" {
  value = oci_core_subnet.subnet.id
}

output "igw_id" {
  value = oci_core_internet_gateway.igw.id
}

output "nat_id" {
  value = oci_core_nat_gateway.nat.id
}