//! Pin the raw Linux ABI and the functions that may reach it.
#![allow(clippy::unwrap_used)]
use std::{collections::BTreeSet, fs, path::Path};

#[test]
fn only_the_kvm_module_owns_raw_resources() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources = BTreeSet::new();
    for item in fs::read_dir(root.join("src")).unwrap() {
        let item = item.unwrap();
        assert!(item.file_type().unwrap().is_file());
        let name = item.file_name().into_string().unwrap();
        let code = fs::read_to_string(item.path()).unwrap();
        let compact: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        for escape in ["#[path", "include!(", "cfg_attr(", "extern\""] {
            assert!(!compact.contains(escape), "{escape} in {name}");
        }
        if name == "kvm.rs" {
            assert_eq!(compact.matches("unsafe{").count(), 3);
            assert_eq!(compact.matches("allow(unsafe_code)").count(), 3);
            for function in ["syscall", "fresh", "bytes"] {
                assert!(compact.contains(&format!("#[allow(unsafe_code)]fn{function}(")));
            }
            let requests = [
                (
                    "GET_SUPPORTED_CPUID",
                    "0xc008ae05",
                    "kvm",
                    "(&rawmutsupported)asusize",
                ),
                (
                    "SET_CPUID2",
                    "0x4008ae90",
                    "cpu",
                    "(&rawconstsupported)asusize",
                ),
                ("GET_SREGS", "0x8138ae83", "cpu", "(&rawmuts)asusize"),
                (
                    "SET_SREGS",
                    "0x4138ae84",
                    "cpu",
                    "input::<KVM_SET_SREGS,_>(&s)",
                ),
                (
                    "SET_REGS",
                    "0x4090ae82",
                    "cpu",
                    "input::<KVM_SET_REGS,_>(&regs)",
                ),
                ("GET_API_VERSION", "0xae00", "&kvm", "0"),
                ("GET_VCPU_MMAP_SIZE", "0xae04", "&kvm", "0"),
                ("CREATE_VM", "0xae01", "&kvm", "0"),
                ("SET_TSS_ADDR", "0xae47", "&vm", "0xfffbd000"),
                (
                    "SET_IDENTITY_MAP_ADDR",
                    "0x4008ae48",
                    "&vm",
                    "input::<KVM_SET_IDENTITY_MAP_ADDR,_>(&identity)",
                ),
                (
                    "SET_USER_MEMORY_REGION",
                    "0x4020ae46",
                    "&vm",
                    "input::<KVM_SET_USER_MEMORY_REGION,_>(&region)",
                ),
                ("CREATE_IRQCHIP", "0xae60", "&vm", "0"),
                (
                    "CREATE_PIT2",
                    "0x4040ae77",
                    "&vm",
                    "input::<KVM_CREATE_PIT2,_>(&pit)",
                ),
                ("CREATE_VCPU", "0xae41", "&vm", "0"),
                ("RUN", "0xae80", "&cpu", "0"),
                (
                    "IRQ_LINE",
                    "0x4008ae61",
                    "vm",
                    "input::<KVM_IRQ_LINE,_>(&irq)",
                ),
            ];
            let compact = compact.replace(",)", ")");
            assert_eq!(compact.matches("ioctl(").count(), 1 + requests.len());
            assert_eq!(compact.matches("constKVM_").count(), requests.len());
            for (name, value, fd, argument) in requests {
                assert!(compact.contains(&format!("constKVM_{name}:usize={value};")));
                assert!(compact.contains(&format!("ioctl({fd},KVM_{name},{argument})")));
            }
            assert_eq!(compact.matches("syscall(").count(), 5);
            assert_eq!(compact.matches("fresh(").count(), 4);
            assert_eq!(compact.matches("Mapping::map(").count(), 2);
            for shape in [
                "syscall(16,[file.as_raw_fd()asusize,request,value,0,0,0])",
                "syscall(9,[0,len,3,1,file.as_raw_fd()asusize,0])",
                "fresh(syscall(319,[name.as_ptr()asusize,1,0,0,0,0])?)",
                "syscall(11,[self.address.as_ptr()asusize,self.len,0,0,0,0])",
                "Mapping::map(len,&file)",
                "Mapping::map(len,&cpu)",
                "fresh(ioctl(&kvm,KVM_CREATE_VM,0)?)",
                "fresh(ioctl(&vm,KVM_CREATE_VCPU,0)?)",
                "std::slice::from_raw_parts_mut(self.address.as_ptr(),self.len)",
            ] {
                assert!(compact.contains(shape), "{shape}");
            }
            assert_eq!(compact.matches("from_raw_fd(").count(), 1);
            assert_eq!(compact.matches("from_raw_parts_mut(").count(), 1);
        } else {
            assert!(!compact.contains("unsafe{") && !compact.contains("allow(unsafe_code)"));
        }
        sources.insert(name);
    }
    assert_eq!(
        sources,
        ["block.rs", "boot.rs", "kvm.rs", "main.rs", "serial.rs"]
            .map(String::from)
            .into()
    );
}
