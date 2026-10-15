variable "compartment_id" {
  type        = string
  description = "OCI compartment OCID"
}

variable "availability_domain" {
  type        = string
  description = "Availability domain for the block volume"
  default     = ""
}

variable "block_volume_size_gbs" {
  type        = number
  description = "Block volume size in GBs"
  default     = 100
}

variable "backup_policy_id" {
  type        = string
  description = "OCI volume backup policy OCID (empty disables assignment)"
  default     = ""
}

variable "namespace" {
  type        = string
  description = "OCI Object Storage namespace"
}

variable "tags" {
  type        = map(string)
  description = "Freeform tags applied to all resources"
  default     = {}
}
