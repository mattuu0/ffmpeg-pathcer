pub mod d3d11_device;
pub mod dxgi_adapter;
pub mod dxgi_device;
pub mod dxgi_output;
pub mod duplication_proxy;
pub mod slots;
pub mod vtable;

/// Installs the full hook chain. Only the D3D11CreateDevice entry point needs
/// installing eagerly; everything downstream (QueryInterface -> GetAdapter ->
/// EnumOutputs -> DuplicateOutput -> AcquireNextFrame) installs itself lazily
/// as each new COM object is observed for the first time.
pub unsafe fn install_all() {
    d3d11_device::install();
}
