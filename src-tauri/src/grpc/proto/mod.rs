// Generated gRPC code re-exported under stable namespaces.
//
// `tonic-prost-build` emits one rust module per protobuf package — i.e.
// `jobworkerp.data` → `jobworkerp.data.rs`, etc. Each module contains the
// generated message types and (for service files) `*Client` types.

#![allow(clippy::all, clippy::pedantic, clippy::nursery)]
#![allow(unused_qualifications, missing_docs, rustdoc::broken_intra_doc_links)]

// Phase C: re-add jobworkerp service/data protos here when wiring the
// named-worker upsert. For now only the runner-level LlmChatResult
// message is exposed so the RAG chat command can decode per-token chunks
// returned by the memories-llm worker (see specs/rag-chat-design.md PR0).

pub mod llm_memory {
    pub mod data {
        tonic::include_proto!("llm_memory.data");
    }
    pub mod service {
        tonic::include_proto!("llm_memory.service");
    }
}

pub mod jobworkerp {
    pub mod runner {
        pub mod llm {
            tonic::include_proto!("jobworkerp.runner.llm");
        }
    }
}

pub mod jobworkerp_conductor {
    pub mod data {
        tonic::include_proto!("jobworkerp_conductor.data");
    }
    pub mod service {
        tonic::include_proto!("jobworkerp_conductor.service");
    }
}
