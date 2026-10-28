use ironrdp_cliprdr::backend::{ClipboardMessageProxy, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_core::impl_as_any;
use tracing::debug;

#[derive(Debug)]
pub struct StubClipboard;

impl StubClipboard {
    pub fn new(_proxy: impl ClipboardMessageProxy + 'static) -> Self {
        Self
    }

    pub fn backend_factory(&self) -> Box<dyn CliprdrBackendFactory + Send> {
        Box::new(StubCliprdrBackendFactory)
    }
}

#[derive(Debug)]
pub struct StubCliprdrBackendFactory;

impl CliprdrBackendFactory for StubCliprdrBackendFactory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(StubCliprdrBackend)
    }
}

#[derive(Debug)]
pub struct StubCliprdrBackend;

impl_as_any!(StubCliprdrBackend);

impl CliprdrBackend for StubCliprdrBackend {
    fn client_capabilities(&self) -> ironrdp_cliprdr::pdu::ClipboardGeneralCapabilityFlags {
        ironrdp_cliprdr::pdu::ClipboardGeneralCapabilityFlags::empty()
    }

    fn temporary_directory(&self) -> &str {
        "."
    }

    fn on_ready(&mut self) {}
    fn on_request_format_list(&mut self) {}
    fn on_process_negotiated_capabilities(
        &mut self,
        _: ironrdp_cliprdr::pdu::ClipboardGeneralCapabilityFlags,
    ) {
    }
    fn on_remote_copy(&mut self, _: &[ironrdp_cliprdr::pdu::ClipboardFormat]) {}
    fn on_format_data_request(&mut self, _: ironrdp_cliprdr::pdu::FormatDataRequest) {}
    fn on_format_data_response(&mut self, _: ironrdp_cliprdr::pdu::FormatDataResponse<'_>) {}
    fn on_file_contents_request(&mut self, _: ironrdp_cliprdr::pdu::FileContentsRequest) {}
    fn on_file_contents_response(&mut self, _: ironrdp_cliprdr::pdu::FileContentsResponse<'_>) {}
    fn on_lock(&mut self, _: ironrdp_cliprdr::pdu::LockDataId) {}
    fn on_unlock(&mut self, _: ironrdp_cliprdr::pdu::LockDataId) {}

    fn now_ms(&self) -> u64 {
        crate::cliprdr::native_now_ms()
    }
}
