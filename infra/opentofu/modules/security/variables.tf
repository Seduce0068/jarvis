variable "compartment_id" {
  type        = string
  description = "OCI compartment OCID"
}

variable "vcn_id" {
  type        = string
  description = "VCN OCID the NSG belongs to"
}

variable "subnet_cidr" {
  type        = string
  description = "Subnet CIDR used as source for internal service rules"
}

variable "allowed_ssh_cidrs" {
  type        = list(string)
  description = "CIDR blocks allowed to reach SSH (empty disables the rule)"
  default     = []
}

variable "allowed_web_cidrs" {
  type        = list(string)
  description = "CIDR blocks allowed to reach code-server / OpenCode Web"
  default     = []
}

variable "tags" {
  type        = map(string)
  description = "Freeform tags applied to all resources"
  default     = {}
}
