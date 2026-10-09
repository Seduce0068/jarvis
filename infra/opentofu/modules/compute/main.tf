# Compute Module

data "oci_core_images" "ubuntu" {
  compartment_id = var.compartment_id
  operating_system = "Canonical Ubuntu"
  operating_system_version = "22.04"
  shape = var.shape
  sort_by = "TIMECREATED"
  sort_order = "DESC"
}

resource "oci_core_instance" "this" {
  compartment_id     = var.compartment_id
  availability_domain = var.availability_domain
  shape              = var.shape
  shape_config {
    ocpus         = var.ocpus
    memory_in_gbs = var.memory_in_gbs
  }

  display_name = "jarvis-host"
  hostname_label = "jarvis"

  source_details {
    source_type = "image"
    source_id   = data.oci_core_images.ubuntu.images[0].id
    boot_volume_size_in_gbs = var.boot_volume_size_gbs
  }

  create_vnic_details {
    subnet_id        = var.subnet_id
    nsg_ids          = var.nsg_ids
    assign_public_ip = true
    hostname_label   = "jarvis"
  }

  metadata = {
    "ssh_authorized_keys" = var.ssh_public_key
    "user_data"           = base64encode(file(var.cloud_init_path))
  }

  freeform_tags = var.tags

  # Preemptible for cost savings (Always Free compatible)
  instance_configuration {
    # Use capacity reservation if available
  }
}

# Attach block volume
resource "oci_core_volume" "data" {
  compartment_id      = var.compartment_id
  availability_domain = var.availability_domain
  size_in_gbs         = var.block_volume_size_gbs
  display_name        = "jarvis-data"
  vpus_per_gb         = 10

  freeform_tags = var.tags
}

resource "oci_core_volume_attachment" "data" {
  instance_id = oci_core_instance.this.id
  volume_id   = oci_core_volume.data.id
  type        = "paravirtualized"
}

output "public_ip" {
  value = oci_core_instance.this.public_ip
}

output "private_ip" {
  value = oci_core_instance.this.private_ip
}

output "instance_id" {
  value = oci_core_instance.this.id
}

output "volume_id" {
  value = oci_core_volume.data.id
}