//! Google Cloud Firestore v1 gRPC types and service stubs, generated from the official protos.

#![allow(clippy::all, missing_docs, rustdoc::all)]

pub mod google {
    pub mod api {
        tonic::include_proto!("google.api");
    }
    pub mod rpc {
        tonic::include_proto!("google.rpc");
    }
    pub mod r#type {
        tonic::include_proto!("google.r#type");
    }
    pub mod firestore {
        pub mod v1 {
            tonic::include_proto!("google.firestore.v1");
        }
    }
}

pub use google::firestore::v1;

pub mod json;
