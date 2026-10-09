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

# Object Storage bucket for backups
resource "oci_objectstorage_bucket" "backups" {
  compartment_id = var.compartment_id
  name           = "jarvis-backups-${random_id.suffix.hex}"
  namespace      = var.namespace
  storage_tier   = "Standard"
  versioning     = "Enabled"

  freeform_tags = var.tags
}

# Object Storage bucket for logs
resource "oci_objectstorage_bucket" "logs" {
  compartment_id = var.compartment_id
  name           = "jarvis-logs-${random_id.suffix.hex}"
  namespace      = var.namespace
  storage_tier   = "Standard"
  versioning     = "Enabled"

  freeform_tags = var.tags
}

resource "random_id" "suffix" {
  byte_length = 4
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