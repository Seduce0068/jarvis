variable "compartment_id" {
  type        = string
  description = "OCI compartment OCID"
}

variable "availability_domain" {
  type        = string
  description = "Availability domain for the instance"
  default     = ""
}

variable "subnet_id" {
  type        = string
  description = "Subnet OCID for the primary VNIC"
}

variable "nsg_ids" {
  type        = list(string)
  description = "Network security group OCIDs attached to the VNIC"
  default     = []
}

variable "shape" {
  type        = string
  description = "Instance shape"
  default     = "VM.Standard.A1.Flex"
}

variable "ocpus" {
  type        = number
  description = "OCPU count"
  default     = 2
}

variable "memory_in_gbs" {
  type        = number
  description = "Memory in GBs"
  default     = 12
}

variable "boot_volume_size_gbs" {
  type        = number
  description = "Boot volume size in GBs"
  default     = 100
}

variable "ssh_public_key" {
  type        = string
  description = "SSH public key injected into the instance"
  default     = ""
}

variable "cloud_init_path" {
  type        = string
  description = "Path to the cloud-init template for the instance"
}

variable "tailscale_auth_key" {
  type        = string
  description = "Tailscale auth key rendered into cloud-init"
  sensitive   = true
  default     = ""
}

variable "tags" {
  type        = map(string)
  description = "Freeform tags applied to all resources"
  default     = {}
}
