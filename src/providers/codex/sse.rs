use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use bytes::Bytes;
use futures::StreamExt;
use reqwest::header::{CONTENT_TYPE, HeaderValue};
use rig::{
    http_client::{
        self, HttpClientExt, LazyBody, MultipartForm, Request, Response, StreamingResponse,
    },
    providers::openai::responses_api,
    wasm_compat::WasmCompatSend,
};

#[derive(Clone, Debug, Default)]
pub(crate) struct CompletionEvidence(Arc<AtomicBool>);

impl CompletionEvidence {
    pub(crate) fn completed(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn reset(&self) {
        self.0.store(false, Ordering::Release);
    }

    fn observe_completed(&self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct CodexHttpClient {
    client: reqwest::Client,
    completion_evidence: CompletionEvidence,
}

impl CodexHttpClient {
    pub(super) fn new(client: reqwest::Client, completion_evidence: CompletionEvidence) -> Self {
        Self {
            client,
            completion_evidence,
        }
    }
}

impl HttpClientExt for CodexHttpClient {
    fn send<T, U>(
        &self,
        request: Request<T>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        T: Into<Bytes> + WasmCompatSend,
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        self.client.send(request)
    }

    fn send_multipart<U>(
        &self,
        request: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        self.client.send_multipart(request)
    }

    fn send_streaming<T>(
        &self,
        request: Request<T>,
    ) -> impl Future<Output = http_client::Result<StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        let client = self.client.clone();
        let completion_evidence = self.completion_evidence.clone();
        async move {
            let mut response = client.send_streaming(request).await?;
            if !response.headers().contains_key(CONTENT_TYPE) {
                response
                    .headers_mut()
                    .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
            }
            completion_evidence.reset();
            let (parts, body) = response.into_parts();
            let observed_body: rig::http_client::sse::BoxedStream = Box::pin(
                futures::stream::unfold((body, Vec::new()), move |(mut body, mut buffer)| {
                    let completion_evidence = completion_evidence.clone();
                    async move {
                        match body.next().await {
                            Some(item) => {
                                if let Ok(bytes) = &item
                                    && observe_completed_sse(&mut buffer, bytes, false)
                                {
                                    completion_evidence.observe_completed();
                                }
                                Some((item, (body, buffer)))
                            }
                            None => {
                                if observe_completed_sse(&mut buffer, &Bytes::new(), true) {
                                    completion_evidence.observe_completed();
                                }
                                None
                            }
                        }
                    }
                }),
            );
            Ok(Response::from_parts(parts, observed_body))
        }
    }
}

fn observe_completed_sse(buffer: &mut Vec<u8>, bytes: &Bytes, end_of_stream: bool) -> bool {
    buffer.extend_from_slice(bytes);
    let mut observed = false;

    while let Some((end, delimiter_len)) = next_sse_event(buffer, end_of_stream) {
        let event = normalize_sse_lines(&buffer[..end]);
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        observed |= matches!(
            serde_json::from_str::<responses_api::streaming::StreamingCompletionChunk>(&data),
            Ok(responses_api::streaming::StreamingCompletionChunk::Response(chunk))
                if matches!(
                    chunk.kind,
                    responses_api::streaming::ResponseChunkKind::ResponseCompleted
                ) && matches!(chunk.response.status, responses_api::ResponseStatus::Completed)
                    && chunk.response.error.is_none()
                    && chunk.response.incomplete_details.is_none()
        );
        buffer.drain(..end + delimiter_len);
    }

    observed
}

fn next_sse_event(buffer: &[u8], end_of_stream: bool) -> Option<(usize, usize)> {
    let mut search_from = 0;
    loop {
        let first = buffer[search_from..]
            .iter()
            .position(|byte| matches!(byte, b'\r' | b'\n'))
            .map(|offset| search_from + offset)?;
        let first_length = sse_line_ending_length(buffer, first, end_of_stream)?;
        let second = first + first_length;
        if second < buffer.len() && matches!(buffer[second], b'\r' | b'\n') {
            let second_length = sse_line_ending_length(buffer, second, end_of_stream)?;
            return Some((first, first_length + second_length));
        }
        search_from = second;
    }
}

fn sse_line_ending_length(buffer: &[u8], index: usize, end_of_stream: bool) -> Option<usize> {
    match (buffer[index], buffer.get(index + 1)) {
        (b'\r', Some(b'\n')) => Some(2),
        (b'\r', Some(_)) | (b'\r', None) if end_of_stream || index + 1 < buffer.len() => Some(1),
        (b'\n', _) => Some(1),
        _ => None,
    }
}

fn normalize_sse_lines(bytes: &[u8]) -> String {
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' {
            normalized.push(b'\n');
            index += usize::from(bytes.get(index + 1) == Some(&b'\n')) + 1;
        } else {
            normalized.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&normalized).into_owned()
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use serde_json::json;

    use super::observe_completed_sse;

    fn completed_event() -> String {
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_test",
                "object": "response",
                "created_at": 0,
                "status": "completed",
                "error": null,
                "incomplete_details": null,
                "instructions": null,
                "max_output_tokens": null,
                "model": "gpt-5.6-luna",
                "usage": {
                    "input_tokens": 1,
                    "output_tokens": 1,
                    "total_tokens": 2
                },
                "output": [],
                "tools": []
            },
            "sequence_number": 1
        })
        .to_string()
    }

    #[test]
    fn completion_observer_accepts_all_sse_line_endings_across_chunk_boundaries() {
        let event = completed_event();
        for (name, encoded) in [
            ("lf", format!("data: {event}\n\n")),
            ("crlf", format!("data: {event}\r\n\r\n")),
            ("cr", format!("data: {event}\r\r")),
            ("mixed-crlf-cr", format!("data: {event}\r\n\r")),
            ("mixed-lf-crlf", format!("data: {event}\n\r\n")),
        ] {
            let mut buffer = Vec::new();
            let mut observed = false;
            for byte in encoded.as_bytes().chunks(1) {
                observed |=
                    observe_completed_sse(&mut buffer, &Bytes::copy_from_slice(byte), false);
            }
            observed |= observe_completed_sse(&mut buffer, &Bytes::new(), true);
            assert!(observed, "{name} line-ending variant was not observed");
            assert!(buffer.is_empty(), "{name} left {buffer:?}");
        }

        let mut buffer = Vec::new();
        let multiline = format!("event: message\rdata: {event}\ndata: \r\n\r");
        let observed = observe_completed_sse(
            &mut buffer,
            &Bytes::copy_from_slice(multiline.as_bytes()),
            true,
        );
        assert!(observed);
        assert!(buffer.is_empty());
    }
}
