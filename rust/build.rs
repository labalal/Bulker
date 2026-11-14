use std::env;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    
    let target_cpu = detect_target_cpu();
    println!("cargo:rustc-cfg=target_cpu=\"{}\"", target_cpu);
    println!("cargo:rustc-env=TARGET_CPU={}", target_cpu);
    
    let num_cpus = num_cpus::get();
    println!("cargo:rustc-env=NUM_CPUS={}", num_cpus);
    
    if let Ok(output) = Command::new("rustc").args(["-vV"]).output() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.starts_with("host: ") {
                println!("cargo:rustc-env=RUST_HOST={}", line.split_once(' ').unwrap().1);
            }
        }
    }
    
    println!("cargo:warning=Detected CPU: {}", target_cpu);
    println!("cargo:warning=CPU cores: {}", num_cpus);
}

fn detect_target_cpu() -> String {
    if let Ok(output) = Command::new("lscpu").output() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut vendor = String::new();
        let mut model_name = String::new();
        let mut flags = String::new();
        
        for line in stdout.lines() {
            if line.starts_with("Vendor ID:") {
                vendor = line.split(':').nth(1).unwrap_or("").trim().to_string();
            } else if line.starts_with("Model name:") {
                model_name = line.split(':').nth(1).unwrap_or("").trim().to_string();
            } else if line.starts_with("Flags:") {
                flags = line.split(':').nth(1).unwrap_or("").trim().to_string();
            }
        }
        
        let cpu = classify_cpu(&vendor, &model_name, &flags);
        if cpu != "native" {
            return cpu;
        }
    }
    
    if let Ok(output) = Command::new("cat").arg("/proc/cpuinfo").output() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut vendor = String::new();
        let mut model_name = String::new();
        let mut flags = String::new();
        
        for line in stdout.lines() {
            if line.starts_with("vendor_id") {
                vendor = line.split(':').nth(1).unwrap_or("").trim().to_string();
            } else if line.starts_with("model name") {
                model_name = line.split(':').nth(1).unwrap_or("").trim().to_string();
            } else if line.starts_with("flags") {
                flags = line.split(':').nth(1).unwrap_or("").trim().to_string();
            }
        }
        
        return classify_cpu(&vendor, &model_name, &flags);
    }
    
    "native".to_string()
}

fn classify_cpu(vendor: &str, model_name: &str, flags: &str) -> String {
    let model_lower = model_name.to_lowercase();
    let vendor_lower = vendor.to_lowercase();
    let flags_lower = flags.to_lowercase();
    
    if vendor_lower.contains("genuineintel") || vendor_lower.contains("intel") {
        if model_lower.contains("panther lake") || model_lower.contains("pantherlake") {
            return "panther-lake".to_string();
        }
        if model_lower.contains("arrow lake") || model_lower.contains("arrowlake") {
            return "arrow-lake".to_string();
        }
        if model_lower.contains("lunar lake") || model_lower.contains("lunarlake") {
            return "lunar-lake".to_string();
        }
        if model_lower.contains("meteor lake") || model_lower.contains("meteorlake") {
            return "meteor-lake".to_string();
        }
        if model_lower.contains("core ultra") {
            if flags_lower.contains("avx512f") && flags_lower.contains("avx512vl") {
                return "meteor-lake".to_string();
            }
            return "arrow-lake".to_string();
        }
        if model_lower.contains("13th gen") || model_lower.contains("14th gen") || 
           model_lower.contains("raptor lake") || model_lower.contains("raptorlake") {
            return "raptor-lake".to_string();
        }
        if model_lower.contains("12th gen") || model_lower.contains("alder lake") || model_lower.contains("alderlake") {
            return "alder-lake".to_string();
        }
        if model_lower.contains("11th gen") || model_lower.contains("tiger lake") || model_lower.contains("tigerlake") {
            return "tiger-lake".to_string();
        }
        if model_lower.contains("10th gen") || model_lower.contains("ice lake") || model_lower.contains("icelake") {
            return "ice-lake".to_string();
        }
        if model_lower.contains("rocket lake") || model_lower.contains("rocketlake") {
            return "rocket-lake".to_string();
        }
        if model_lower.contains("comet lake") || model_lower.contains("cometlake") {
            return "comet-lake".to_string();
        }
        if model_lower.contains("cascade lake") || model_lower.contains("cascadelake") {
            return "cascade-lake".to_string();
        }
        if model_lower.contains("skylake") || model_lower.contains("skx") {
            if flags_lower.contains("avx512f") {
                return "skylake-avx512".to_string();
            }
            return "skylake".to_string();
        }
        if model_lower.contains("broadwell") {
            return "broadwell".to_string();
        }
        if model_lower.contains("haswell") {
            return "haswell".to_string();
        }
        return "nehalem".to_string();
    }
    
    if vendor_lower.contains("authenticamd") || vendor_lower.contains("amd") {
        if model_lower.contains("zen 5") || model_lower.contains("ryzen 9000") || model_lower.contains("granite ridge") {
            return "zen5".to_string();
        }
        if model_lower.contains("zen 4") || model_lower.contains("ryzen 7000") || model_lower.contains("raphael") {
            return "zen4".to_string();
        }
        if model_lower.contains("zen 3") || model_lower.contains("ryzen 5000") || model_lower.contains("vermeer") || model_lower.contains("cactus") {
            return "zen3".to_string();
        }
        if model_lower.contains("zen 2") || model_lower.contains("ryzen 3000") || model_lower.contains("matisse") || model_lower.contains("renoir") {
            return "zen2".to_string();
        }
        if model_lower.contains("zen") || model_lower.contains("ryzen 1000") || model_lower.contains("summit ridge") {
            return "znver1".to_string();
        }
        return "x86-64-v3".to_string();
    }
    
    if vendor_lower.contains("apple") || model_lower.contains("apple") {
        if model_lower.contains("m3") || model_lower.contains("a17") {
            return "apple-m3".to_string();
        }
        if model_lower.contains("m2") || model_lower.contains("a16") {
            return "apple-m2".to_string();
        }
        if model_lower.contains("m1") || model_lower.contains("a14") || model_lower.contains("a15") {
            return "apple-m1".to_string();
        }
        return "apple-m1".to_string();
    }
    
    "native".to_string()
}