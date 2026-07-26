#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MemoryDomain {
    Cpu,
    D3D12,
    Metal,
    Vulkan,
    DmaBuf,
    Shared,
}
