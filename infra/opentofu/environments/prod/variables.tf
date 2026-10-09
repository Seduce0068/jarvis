variable "region" {
  type        = string
  description = "OCI region"
  default     = "ap-tokyo-1"
}

variable "compartment_id" {
  type        = string
  description = "OCI Compartment OCID"
}

variable "availability_domain" {
  type        = string
  description = "Availability Domain"
  default     = ""
}

variable "vcn_cidr" {
  type        = string
  description = "VCN CIDR block"
  default     = "10.0.0.0/16"
}

variable "subnet_cidr" {
  type        = string
  description = "Subnet CIDR block"
  default     = "10.0.1.0/24"
}

variable "instance_shape" {
  type        = string
  description = "Instance shape"
  default     = "VM.Standard.A1.Flex"
}

variable "instance_ocpus" {
  type        = number
  description = "OCPU count"
  default     = 2
}

variable "instance_memory_gbs" {
  type        = number
  description = "Memory in GBs"
  default     = 12
}

variable "boot_volume_size_gbs" {
  type        = number
  description = "Boot volume size in GBs"
  default     = 100
}

variable "block_volume_size_gbs" {
  type        = number
  description = "Block volume size in GBs"
  default     = 100
}

variable "backup_policy_id" {
  type        = string
  description = "Backup policy OCID (optional)"
  default     = ""
}

variable "ssh_public_key" {
  type        = string
  description = "SSH public key for instance access"
}

variable "ssh_private_key_path" {
  type        = string
  description = "Path to SSH private key for output"
  default     = "~/.ssh/id_ed25519"
}

variable "allowed_ssh_cidrs" {
  type        = list(string)
  description = "CIDR blocks allowed for SSH access"
  default     = []
}

variable "allowed_web_cidrs" {
  type        = list(string)
  description = "CIDR blocks allowed for web access (code-server, OpenCode Web)"
  default     = []
}

variable "tailscale_auth_key" {
  type        = string
  description = "Tailscale auth key"
  sensitive   = true
}

variable "common_tags" {
  type = map(string)
  default = {
    project     = "jarvis"
    environment = "prod"
    managed_by  = "opentofu"
  }
}