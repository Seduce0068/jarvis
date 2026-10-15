# OpenTofu Configuration for JARVIS on OCI
# Root module for production environment

terraform {
  required_version = ">= 1.6"
  required_providers {
    oci = {
      source  = "oracle/oci"
      version = "~> 5.0"
    }
  }

  backend "local" {
    path = "terraform.tfstate"
  }
}

provider "oci" {
  region = var.region
  # Authentication via ~/.oci/config or environment variables
}

# Object Storage namespace (used for backup/log buckets)
data "oci_objectstorage_namespace" "this" {
  compartment_id = var.compartment_id
}

# Network module
module "network" {
  source = "../../modules/network"

  compartment_id      = var.compartment_id
  vcn_cidr            = var.vcn_cidr
  subnet_cidr         = var.subnet_cidr
  availability_domain = var.availability_domain

  tags = var.common_tags
}

# Security module
module "security" {
  source = "../../modules/security"

  compartment_id = var.compartment_id
  vcn_id         = module.network.vcn_id
  subnet_cidr    = var.subnet_cidr

  allowed_ssh_cidrs = var.allowed_ssh_cidrs
  allowed_web_cidrs = var.allowed_web_cidrs

  tags = var.common_tags
}

# Compute module
module "compute" {
  source = "../../modules/compute"

  compartment_id      = var.compartment_id
  availability_domain = var.availability_domain
  subnet_id           = module.network.subnet_id
  nsg_ids             = [module.security.nsg_id]

  shape                = var.instance_shape
  ocpus                = var.instance_ocpus
  memory_in_gbs        = var.instance_memory_gbs
  boot_volume_size_gbs = var.boot_volume_size_gbs

  ssh_public_key     = var.ssh_public_key
  cloud_init_path    = "${path.module}/cloud-init.yaml"
  tailscale_auth_key = var.tailscale_auth_key

  tags = var.common_tags
}

# Storage module (data volume + Object Storage buckets)
module "storage" {
  source = "../../modules/storage"

  compartment_id      = var.compartment_id
  availability_domain = var.availability_domain

  block_volume_size_gbs = var.block_volume_size_gbs
  backup_policy_id      = var.backup_policy_id
  namespace             = data.oci_objectstorage_namespace.this.namespace

  tags = var.common_tags
}

# Attach the data volume to the instance
resource "oci_core_volume_attachment" "data" {
  instance_id     = module.compute.instance_id
  volume_id       = module.storage.data_volume_id
  attachment_type = "paravirtualized"
}

# Observability module
module "observability" {
  source = "../../modules/observability"

  compartment_id = var.compartment_id
  instance_id    = module.compute.instance_id

  tags = var.common_tags
}

# Outputs
output "instance_public_ip" {
  value       = module.compute.public_ip
  description = "Public IP of the JARVIS instance"
}

output "instance_private_ip" {
  value       = module.compute.private_ip
  description = "Private IP of the JARVIS instance"
}

output "backups_bucket" {
  value       = module.storage.backups_bucket
  description = "Object Storage bucket for backups"
}

output "logs_bucket" {
  value       = module.storage.logs_bucket
  description = "Object Storage bucket for logs"
}

output "ssh_command" {
  value = "ssh -i ${var.ssh_private_key_path} ubuntu@${module.compute.public_ip}"
}
