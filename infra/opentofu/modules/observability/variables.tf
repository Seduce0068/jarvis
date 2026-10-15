variable "compartment_id" {
  type        = string
  description = "OCI compartment OCID"
}

variable "instance_id" {
  type        = string
  description = "Instance OCID the alarms watch"
}

variable "tags" {
  type        = map(string)
  description = "Freeform tags applied to all resources"
  default     = {}
}
