terraform {
  required_providers {
    oci = {
      source  = "oracle/oci"
      version = "~> 5.0"
    }
  }
}

# Observability Module

# Notification topic for alarm deliveries
resource "oci_ons_notification_topic" "jarvis" {
  compartment_id = var.compartment_id
  name           = "jarvis-alerts"

  freeform_tags = var.tags
}

# OCI Logging
resource "oci_logging_log_group" "jarvis" {
  compartment_id = var.compartment_id
  display_name   = "jarvis-log-group"
  description    = "Log group for JARVIS services"

  freeform_tags = var.tags
}

resource "oci_logging_log" "jarvis_services" {
  log_group_id = oci_logging_log_group.jarvis.id
  display_name = "jarvis-services"
  log_type     = "SERVICE"

  configuration {
    compartment_id = var.compartment_id

    source {
      category    = "all"
      resource    = var.instance_id
      service     = "compute"
      source_type = "OCISERVICE"
    }
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
  namespace             = "oci_computeagent"
  query                 = "CpuUtilization[1m]{resourceId=\"${var.instance_id}\"}.mean() > 90"

  destinations                 = [oci_ons_notification_topic.jarvis.id]
  repeat_notification_duration = "PT1H"

  freeform_tags = var.tags
}

# Alarm for Idle Compute detection (CPU < 5% for 7 days)
resource "oci_monitoring_alarm" "idle_compute" {
  compartment_id = var.compartment_id
  display_name   = "JARVIS Idle Compute Risk"
  is_enabled     = true
  severity       = "WARNING"

  metric_compartment_id = var.compartment_id
  namespace             = "oci_computeagent"
  query                 = "CpuUtilization[1d]{resourceId=\"${var.instance_id}\"}.mean() < 5"

  destinations                 = [oci_ons_notification_topic.jarvis.id]
  repeat_notification_duration = "P1D"

  freeform_tags = var.tags
}
