# OpenTofu Configuration for JARVIS on OCI
# Root module for production environment

terraform {
  required_version = ">= 1.6"
  required_providers {
    oci = {
      source  = "oracle/oci"
      version = "~> 5.0"
    }
    tls = {
      source  = "hashicorp/tls"
      version = "~> 4.0"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.6"
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

# Random suffix for unique resource names
resource "random_id" "suffix" {
  byte_length = 4
}

# Network module
module "network" {
  source = "./modules/network"
  
  compartment_id = var.compartment_id
  vcn_cidr       = var.vcn_cidr
  subnet_cidr    = var.subnet_cidr
  availability_domain = var.availability_domain
  
  tags = var.common_tags
}

# Security module
module "security" {
  source = "./modules/security"
  
  compartment_id = var.compartment_id
  vcn_id         = module.network.vcn_id
  subnet_id      = module.network.subnet_id
  
  allowed_ssh_cidrs  = var.allowed_ssh_cidrs
  allowed_web_cidrs  = var.allowed_web_cidrs
  
  tags = var.common_tags
}

# Compute module
module "compute" {
  source = "./modules/compute"
  
  compartment_id     = var.compartment_id
  availability_domain = var.availability_domain
  subnet_id          = module.network.subnet_id
  nsg_ids            = [module.security.nsg_id]
  
  shape                = var.instance_shape
  ocpus                = var.instance_ocpus
  memory_in_gbs        = var.instance_memory_gbs
  boot_volume_size_gbs = var.boot_volume_size_gbs
  
  ssh_public_key      = var.ssh_public_key
  cloud_init_path     = "${path.module}/cloud-init.yaml"
  
  tags = var.common_tags
}

# Storage module
module "storage" {
  source = "./modules/storage"
  
  compartment_id = var.compartment_id
  availability_domain = var.availability_domain
  
  block_volume_size_gbs = var.block_volume_size_gbs
  backup_policy_id      = var.backup_policy_id
  
  tags = var.common_tags
}

# Observability module
module "observability" {
  source = "./modules/observability"
  
  compartment_id = var.compartment_id
  
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

output "tailscale_auth_key" {
  value       = var.tailscale_auth_key
  description = "Tailscale auth key (sensitive)"
  sensitive   = true
}

output "ssh_command" {
  value = "ssh -i ${var.ssh_private_key_path} ubuntu@${module.compute.public_ip}"
}