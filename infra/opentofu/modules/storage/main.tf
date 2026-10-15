terraform {
  required_providers {
    oci = {
      source  = "oracle/oci"
      version = "~> 5.0"
    }
  }
}

# Storage Module

# Block volume for persistent data (PostgreSQL, Qdrant, backups)
resource "oci_core_volume" "data" {
  compartment_id      = var.compartment_id
  availability_domain = var.availability_domain
  size_in_gbs         = var.block_volume_size_gbs
  display_name        = "jarvis-data"
  vpus_per_gb         = 10

  freeform_tags = var.tags
}

# Optional backup policy assignment
resource "oci_core_volume_backup_policy_assignment" "data" {
  count = var.backup_policy_id == "" ? 0 : 1

  asset_id  = oci_core_volume.data.id
  policy_id = var.backup_policy_id
}

# Object Storage bucket for backups (stable name: referenced by backup.rs)
resource "oci_objectstorage_bucket" "backups" {
  compartment_id = var.compartment_id
  name           = "jarvis-backups"
  namespace      = var.namespace
  storage_tier   = "Standard"
  versioning     = "Enabled"

  freeform_tags = var.tags
}

# Object Storage bucket for logs
resource "oci_objectstorage_bucket" "logs" {
  compartment_id = var.compartment_id
  name           = "jarvis-logs"
  namespace      = var.namespace
  storage_tier   = "Standard"
  versioning     = "Enabled"

  freeform_tags = var.tags
}

output "data_volume_id" {
  value = oci_core_volume.data.id
}

output "backups_bucket" {
  value = oci_objectstorage_bucket.backups.name
}

output "logs_bucket" {
  value = oci_objectstorage_bucket.logs.name
}
