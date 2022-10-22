use quote::quote;
use syn::{parse_macro_input, DeriveInput};

#[proc_macro_derive(LocalQueue)]
pub fn derive_local_queue(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
  let input = parse_macro_input!(input as DeriveInput);
  let ident = &input.ident;

  let expanded = quote! {
    impl stack_queue::LocalQueue for #ident {
      fn queue() -> &'static std::thread::LocalKey<stack_queue::StackQueue<Self>> {
        thread_local! {
          static QUEUE: stack_queue::StackQueue<#ident> = stack_queue::StackQueue::new();
        }

        &QUEUE
      }
    }
  };

  expanded.into()
}
