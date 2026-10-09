# Observability Module

# OCI Logging
resource "oci_logging_log_group" "jarvis" {
  compartment_id = var.compartment_id
  display_name   = "jarvis-log-group"
  description    = "Log group for JARVIS services"

  freeform_tags = var.tags
}

resource "oci_logging_log" "jarvis_services" {
  compartment_id = var.compartment_id
  log_group_id   = oci_logging_log_group.jarvis.id
  display_name   = "jarvis-services"
  log_type       = "SERVICE"

  configuration {
    source = "OCI_SERVICE"
    service = "ALL"
  }

  freeform_tags = var.tags
}

# OCI Monitoring Alarm for Instance Health
resource "oci_monitoring_alarm" "instance_health" {
  compartment_id = var.compartment_id
  display_name   = "JARVIS Instance Health"
  is_enabled     = true
  severity       = "CRITICAL"

  metric_compartment_id = var.compartment_id
  namespace            = "oci_computeagent"
  query                = "CpuUtilization[1m]{resourceId=\"${var.instance_id}\"}.mean() > 90"

  destinations = [var.notification_topic_id]
  repeat_notification_duration = "PT1H"
  suppression_duration = "PT5M"

  freeform_tags = var.tags
}

# Alarm for Idle Compute detection (CPU < 5% for 7 days)
resource "oci_monitoring_alarm" "idle_compute" {
  compartment_id = var.compartment_id
  display_name   = "JARVIS Idle Compute Risk"
  is_enabled     = true
  severity       = "WARNING"

  metric_compartment_id = var.compartment_id
  namespace            = "oci_computeagent"
  query                = "CpuUtilization[1d]{resourceId=\"${var.instance_id}\"}.mean() < 5"

  destinations = [var.notification_topic_id]
  repeat_notification_duration = "P1D"
  suppression_duration = "PT1H"

  freeform_tags = var.tags
}

variable "instance_id" {
  type = string
}

variable "notification_topic_id" {
  type = string
}

variable "namespace" {
  type = string
}