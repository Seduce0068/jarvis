# JARVIS Architecture Calculations
# Location: docs/architecture/calc.rs
# Run: rust-script calc.rs

use std::fmt;

/// OCI Always Free resource limits
struct OciLimits {
    ocpus: f64,
    memory_gb: f64,
    block_storage_gb: f64,
    object_storage_tb: f64,
    ocpu_hours_per_month: f64,
    gb_hours_per_month: f64,
}

impl OciLimits {
    fn always_free() -> Self {
        Self {
            ocpus: 2.0,
            memory_gb: 12.0,
            block_storage_gb: 200.0,
            object_storage_tb: 10.0,
            ocpu_hours_per_month: 1500.0,
            gb_hours_per_month: 9000.0,
        }
    }
}

/// JARVIS service resource requirements
#[derive(Debug)]
struct ServiceRequirements {
    name: &'static str,
    ocpus: f64,
    memory_gb: f64,
    disk_gb: f64,
    cpu_weight: f64, // relative CPU intensity 0.0-1.0
}

const SERVICES: &[ServiceRequirements] = &[
    ServiceRequirements { name: "jarvis-policy",   ocpus: 0.25, memory_gb: 1.0, disk_gb: 0.5, cpu_weight: 0.1 },
    ServiceRequirements { name: "jarvis-mcp-gw",   ocpus: 0.25, memory_gb: 1.0, disk_gb: 0.5, cpu_weight: 0.2 },
    ServiceRequirements { name: "jarvis-event",    ocpus: 0.15, memory_gb: 0.5, disk_gb: 0.3, cpu_weight: 0.1 },
    ServiceRequirements { name: "jarvis-context",  ocpus: 0.25, memory_gb: 1.5, disk_gb: 2.0, cpu_weight: 0.15 },
    ServiceRequirements { name: "opencode",        ocpus: 0.50, memory_gb: 2.0, disk_gb: 1.0, cpu_weight: 0.3 },
    ServiceRequirements { name: "code-server",     ocpus: 0.30, memory_gb: 1.5, disk_gb: 1.0, cpu_weight: 0.2 },
    ServiceRequirements { name: "postgresql",      ocpus: 0.20, memory_gb: 2.0, disk_gb: 10.0, cpu_weight: 0.1 },
    ServiceRequirements { name: "qdrant",          ocpus: 0.15, memory_gb: 1.0, disk_gb: 5.0,  cpu_weight: 0.1 },
    ServiceRequirements { name: "otelcol",         ocpus: 0.05, memory_gb: 0.3, disk_gb: 0.1, cpu_weight: 0.05 },
    ServiceRequirements { name: "system/os",       ocpus: 0.10, memory_gb: 1.0, disk_gb: 5.0,  cpu_weight: 0.05 },
];

fn main() {
    let limits = OciLimits::always_free();
    
    // Calculate totals
    let total_ocpus: f64 = SERVICES.iter().map(|s| s.ocpus).sum();
    let total_memory: f64 = SERVICES.iter().map(|s| s.memory_gb).sum();
    let total_disk: f64 = SERVICES.iter().map(|s| s.disk_gb).sum();
    let weighted_cpu: f64 = SERVICES.iter().map(|s| s.ocpus * s.cpu_weight).sum();
    
    // Utilization percentages
    let ocpu_util = (total_ocpus / limits.ocpus) * 100.0;
    let mem_util = (total_memory / limits.memory_gb) * 100.0;
    let disk_util = (total_disk / limits.block_storage_gb) * 100.0;
    
    // Monthly usage (24/7)
    let hours_per_month = 24.0 * 30.0;
    let monthly_ocpu_hours = total_ocpus * hours_per_month;
    let monthly_gb_hours = total_memory * hours_per_month;
    let ocpu_hours_util = (monthly_ocpu_hours / limits.ocpu_hours_per_month) * 100.0;
    let gb_hours_util = (monthly_gb_hours / limits.gb_hours_per_month) * 100.0;
    
    // Headroom
    let ocpu_headroom = limits.ocpus - total_ocpus;
    let mem_headroom = limits.memory_gb - total_memory;
    let disk_headroom = limits.block_storage_gb - total_disk;
    
    // Output as Markdown table
    println!("| Service | OCPUs | Memory (GB) | Disk (GB) | CPU Weight |");
    println!("|---------|-------|-------------|-----------|------------|");
    for svc in SERVICES {
        println!("| {} | {:.2} | {:.1} | {:.1} | {:.2} |", 
            svc.name, svc.ocpus, svc.memory_gb, svc.disk_gb, svc.cpu_weight);
    }
    println!("| **Total** | **{:.2}** | **{:.1}** | **{:.1}** | **{:.2}** |", 
        total_ocpus, total_memory, total_disk, weighted_cpu);
    println!();
    
    println!("## OCI Always Free Utilization");
    println!();
    println!("| Resource | Limit | Used | Utilization | Headroom |");
    println!("|----------|-------|------|-------------|----------|");
    println!("| OCPUs | {:.1} | {:.2} | {:.1}% | {:.2} |", limits.ocpus, total_ocpus, ocpu_util, ocpu_headroom);
    println!("| Memory (GB) | {:.1} | {:.1} | {:.1}% | {:.1} |", limits.memory_gb, total_memory, mem_util, mem_headroom);
    println!("| Block Storage (GB) | {:.1} | {:.1} | {:.1}% | {:.1} |", limits.block_storage_gb, total_disk, disk_util, disk_headroom);
    println!("| Object Storage (TB) | {:.1} | — | — | {:.1} |", limits.object_storage_tb, limits.object_storage_tb);
    println!("| OCPU Hours/Month | {:.0} | {:.0} | {:.1}% | — |", limits.ocpu_hours_per_month, monthly_ocpu_hours, ocpu_hours_util);
    println!("| GB Hours/Month | {:.0} | {:.0} | {:.1}% | — |", limits.gb_hours_per_month, monthly_gb_hours, gb_hours_util);
    println!();
    
    // Cost projection if exceeding free tier
    println!("## Cost Projection (Pay-as-you-go rates, Tokyo region)");
    println!();
    println!("| Resource | Free Tier | Excess | Unit Cost | Monthly Cost |");
    println!("|----------|-----------|--------|-----------|--------------|");
    
    let ocpu_cost_per_hour = 0.0085; // USD per OCPU hour (A1 Flex)
    let mem_cost_per_gb_hour = 0.0012; // USD per GB hour
    
    let excess_ocpus = (total_ocpus - limits.ocpus).max(0.0);
    let excess_mem_gb = (total_memory - limits.memory_gb).max(0.0);
    
    let ocpu_monthly_cost = excess_ocpus * hours_per_month * ocpu_cost_per_hour;
    let mem_monthly_cost = excess_mem_gb * hours_per_month * mem_cost_per_gb_hour;
    let total_monthly = ocpu_monthly_cost + mem_monthly_cost;
    
    println!("| OCPUs | 2.0 | {:.2} | ${:.4}/hr | ${:.2} |", excess_ocpus, ocpu_cost_per_hour, ocpu_monthly_cost);
    println!("| Memory (GB) | 12.0 | {:.1} | ${:.4}/GB-hr | ${:.2} |", excess_mem_gb, mem_cost_per_gb_hour, mem_monthly_cost);
    println!("| **Total** | | | | **${:.2}/month** |", total_monthly);
    println!();
    
    // LaTeX formulas for documentation
    println!("## Key Formulas");
    println!();
    println!("Total OCPU utilization:");
    println!("$$");
    println!("U_{{OCPU}} = \\frac{{\\sum_{{i}} OCPU_i}}{{OCPU_{{limit}}}} \\times 100\\% = \\frac{{{:.2}}}{{2.0}} \\times 100\\% = {:.1}\\%", total_ocpus, ocpu_util);
    println!("$$");
    println!();
    println!("Monthly OCPU hours:");
    println!("$$");
    println!("H_{{OCPU}} = OCPU_{{total}} \\times 24 \\times 30 = {:.2} \\times 720 = {:.0} \\text{{ hours}}", total_ocpus, monthly_ocpu_hours);
    println!("$$");
    println!();
    println!("Cost projection (if exceeding free tier):");
    println!("$$");
    println!("C = \\max(0, OCPU_{{used}} - OCPU_{{free}}) \\times 720 \\times \\$0.0085 + \\max(0, Mem_{{used}} - Mem_{{free}}) \\times 720 \\times \\$0.0012");
    println!("$$");
    println!();
    
    // Verdict
    println!("## Verdict");
    println!();
    if ocpu_util <= 100.0 && mem_util <= 100.0 && disk_util <= 100.0 {
        println!("✅ **All resources within Always Free limits**");
        println!();
        println!("- OCPU headroom: **{:.2} OCPUs** ({:.0}% free)", ocpu_headroom, (ocpu_headroom / limits.ocpus) * 100.0);
        println!("- Memory headroom: **{:.1} GB** ({:.0}% free)", mem_headroom, (mem_headroom / limits.memory_gb) * 100.0);
        println!("- Disk headroom: **{:.1} GB** ({:.0}% free)", disk_headroom, (disk_headroom / limits.block_storage_gb) * 100.0);
    } else {
        println!("⚠️ **Exceeds Always Free limits** — estimated cost: **${:.2}/month**", total_monthly);
    }
}