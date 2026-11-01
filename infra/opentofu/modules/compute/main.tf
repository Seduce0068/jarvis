terraform {
  required_providers {
    oci = {
      source  = "oracle/oci"
      version = "~> 5.0"
    }
  }
}

# Compute Module

data "oci_core_images" "ubuntu" {
  compartment_id           = var.compartment_id
  operating_system         = "Canonical Ubuntu"
  operating_system_version = "22.04"
  shape                    = var.shape
  sort_by                  = "TIMECREATED"
  sort_order               = "DESC"
}

resource "oci_core_instance" "this" {
  compartment_id      = var.compartment_id
  availability_domain = var.availability_domain
  shape               = var.shape
  shape_config {
    ocpus         = var.ocpus
    memory_in_gbs = var.memory_in_gbs
  }

  display_name = "jarvis-host"

  source_details {
    source_type             = "image"
    source_id               = data.oci_core_images.ubuntu.images[0].id
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
    "user_data" = base64gzip(templatefile(var.cloud_init_path, {
      ssh_public_key     = var.ssh_public_key
      tailscale_auth_key = var.tailscale_auth_key
      systemd_dir        = abspath("${path.module}/../../../systemd")
      config_dir         = abspath("${path.module}/../../../config")
      backup_script      = abspath("${path.module}/../../../../rust/scripts/backup.rs")
    }))
  }

  freeform_tags = var.tags
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
