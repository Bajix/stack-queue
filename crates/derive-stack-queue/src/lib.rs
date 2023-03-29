use darling::FromMeta;
use proc_macro2::Span;
use quote::quote;
use syn::{parse::Error, parse_macro_input, AttributeArgs, ImplItem, ItemImpl};

const MIN_BUFFER_LEN: usize = 64;

#[cfg(target_pointer_width = "64")]
const MAX_BUFFER_LEN: usize = u32::MAX as usize;
#[cfg(target_pointer_width = "32")]
const MAX_BUFFER_LEN: usize = u16::MAX as usize;

enum Variant {
  TaskQueue,
  BackgroundQueue,
  BatchReducer,
}
#[derive(FromMeta)]
#[darling(default)]
struct QueueOpt {
  buffer_size: usize,
}

impl Default for QueueOpt {
  fn default() -> Self {
    QueueOpt { buffer_size: 512 }
  }
}

/// derive [LocalQueue](https://docs.rs/stack-queue/latest/stack_queue/trait.LocalQueue.html) from [TaskQueue](https://docs.rs/stack-queue/latest/stack_queue/trait.TaskQueue.html), [BackgroundQueue](https://docs.rs/stack-queue/latest/stack_queue/trait.BackgroundQueue.html) or [BatchReducer](https://docs.rs/stack-queue/latest/stack_queue/trait.BatchReducer.html) impl
#[proc_macro_attribute]
pub fn local_queue(
  args: proc_macro::TokenStream,
  input: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
  let attr_args = parse_macro_input!(args as AttributeArgs);
  let mut input = parse_macro_input!(input as ItemImpl);

  input.attrs = vec![];

  let ident = &input.self_ty;

  let QueueOpt { buffer_size } = match QueueOpt::from_list(&attr_args) {
    Ok(attr) => attr,
    Err(err) => {
      return err.write_errors().into();
    }
  };

  if buffer_size > MAX_BUFFER_LEN {
    return Error::new(
      Span::call_site(),
      format!("buffer_size must not exceed {MAX_BUFFER_LEN}"),
    )
    .into_compile_error()
    .into();
  }

  if buffer_size < MIN_BUFFER_LEN {
    return Error::new(
      Span::call_site(),
      format!("buffer_size must be at least {MIN_BUFFER_LEN}"),
    )
    .into_compile_error()
    .into();
  }

  if buffer_size.ne(&buffer_size.next_power_of_two()) {
    return Error::new(Span::call_site(), "buffer_size must be a power of 2")
      .into_compile_error()
      .into();
  }

  let variant = match &input.trait_ {
    Some((_, path, _)) => {
      let segments: Vec<_> = path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect();

      match *segments
        .iter()
        .map(String::as_ref)
        .collect::<Vec<&str>>()
        .as_slice()
      {
        ["stack_queue", "TaskQueue"] => Some(Variant::TaskQueue),
        ["TaskQueue"] => Some(Variant::TaskQueue),
        ["stack_queue", "BackgroundQueue"] => Some(Variant::BackgroundQueue),
        ["BackgroundQueue"] => Some(Variant::BackgroundQueue),
        ["stack_queue", "BatchReducer"] => Some(Variant::BatchReducer),
        ["BatchReducer"] => Some(Variant::BatchReducer),
        _ => None,
      }
    }
    None => None,
  };

  let variant = match variant {
    Some(variant) => variant,
    None => {
      return Error::new(
        Span::call_site(),
        "must be used on TaskQueue, BackgroundQueue or BatchReducer",
      )
      .into_compile_error()
      .into();
    }
  };

  let task = match input
    .items
    .iter()
    .filter_map(|impl_item| {
      if let ImplItem::Type(impl_type) = impl_item {
        Some(impl_type)
      } else {
        None
      }
    })
    .find(|impl_type| impl_type.ident == "Task")
    .map(|task_impl| &task_impl.ty)
  {
    Some(impl_type) => impl_type,
    None => {
      return Error::new(Span::call_site(), "missing `Task` in implementation")
        .into_compile_error()
        .into();
    }
  };

  let buffer_cell = match &variant {
    Variant::TaskQueue => quote!(stack_queue::task::TaskRef<#ident>),
    Variant::BackgroundQueue => quote!(stack_queue::BufferCell<#task>),
    Variant::BatchReducer => quote!(stack_queue::BufferCell<#task>),
  };

  let queue = quote!(stack_queue::StackQueue<#buffer_cell, #buffer_size>);

  let queue_impl = match &variant {
    Variant::TaskQueue | Variant::BackgroundQueue => quote!(
      #[stack_queue::async_t::async_trait]
      #input
    ),
    Variant::BatchReducer => quote!(
      impl stack_queue::BatchReducer for #ident {
        type Task = #task;
      }
    ),
  };

  let expanded = quote!(
    #queue_impl

    #[cfg(not(loom))]
    impl stack_queue::LocalQueue<#buffer_size> for #ident {
      type BufferCell = #buffer_cell;

      fn queue() -> &'static std::thread::LocalKey<#queue> {
        thread_local! {
          static QUEUE: #queue = stack_queue::StackQueue::default();
        }

        &QUEUE
      }
    }

    #[cfg(loom)]
    impl stack_queue::LocalQueue<#buffer_size> for #ident {
      type BufferCell = #buffer_cell;

      fn queue() -> &'static stack_queue::loom::thread::LocalKey<#queue> {
        stack_queue::loom::thread_local! {
          static QUEUE: #queue = stack_queue::StackQueue::default();
        }

        &QUEUE
      }
    }
  );

  expanded.into()
}
