variable "compartment_id" {
  type        = string
  description = "OCI compartment OCID"
}

variable "availability_domain" {
  type        = string
  description = "Availability domain for the subnet"
  default     = ""
}

variable "vcn_cidr" {
  type        = string
  description = "CIDR block for the VCN"
}

variable "subnet_cidr" {
  type        = string
  description = "CIDR block for the public subnet"
}

variable "tags" {
  type        = map(string)
  description = "Freeform tags applied to all resources"
  default     = {}
}
