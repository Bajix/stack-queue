use darling::FromDeriveInput;
use quote::quote;
use syn::{parse_macro_input, DeriveInput};

#[derive(FromDeriveInput)]
#[darling(attributes(local_queue))]
#[darling(default)]
struct LocalQueueOpt {
  buffer_size: usize,
}

impl Default for LocalQueueOpt {
  fn default() -> Self {
    LocalQueueOpt { buffer_size: 2048 }
  }
}

#[proc_macro_derive(LocalQueue, attributes(local_queue))]
pub fn derive_local_queue(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
  let input = parse_macro_input!(input as DeriveInput);
  let ident = &input.ident;

  let LocalQueueOpt { buffer_size } = match FromDeriveInput::from_derive_input(&input) {
    Ok(attr) => attr,
    Err(err) => {
      return err.write_errors().into();
    }
  };

  let expanded = quote! {
    stack_queue::sa::const_assert!(#buffer_size >= stack_queue::MIN_BUFFER_LEN);
    stack_queue::sa::const_assert!(#buffer_size <= stack_queue::MAX_BUFFER_LEN);
    stack_queue::sa::const_assert_eq!(#buffer_size, #buffer_size.next_power_of_two());

    #[cfg(not(loom))]
    impl stack_queue::LocalQueue<#buffer_size> for #ident {
      fn queue() -> &'static std::thread::LocalKey<stack_queue::StackQueue<Self, #buffer_size>> {
        thread_local! {
          static QUEUE: stack_queue::StackQueue<#ident, #buffer_size> = stack_queue::StackQueue::default();
        }

        &QUEUE
      }
    }

    #[cfg(loom)]
    impl stack_queue::LocalQueue<#buffer_size> for #ident {
      fn queue() -> &'static stack_queue::loom::thread::LocalKey<stack_queue::StackQueue<Self, #buffer_size>> {
        stack_queue::loom::thread_local! {
          static QUEUE: stack_queue::StackQueue<#ident, #buffer_size> = stack_queue::StackQueue::default();
        }

        &QUEUE
      }
    }
  };

  expanded.into()
}
