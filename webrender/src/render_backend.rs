/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use api::{ApiMsg, BuiltDisplayList, ClearCache, DebugCommand};
#[cfg(feature = "debugger")]
use api::{BuiltDisplayListIter, SpecificDisplayItem};
use api::{DeviceIntPoint, DevicePixelScale, DeviceUintPoint, DeviceUintRect, DeviceUintSize};
use api::{DocumentId, DocumentLayer, ExternalScrollId, FrameMsg, HitTestFlags, HitTestResult};
use api::{IdNamespace, LayoutPoint, PipelineId, RenderNotifier, SceneMsg, ScrollClamping};
use api::{MemoryReport, VoidPtrToSizeFn};
use api::{ScrollLocation, ScrollNodeState, TransactionMsg, ResourceUpdate, ImageKey};
use api::{NotificationRequest, Checkpoint};
use api::channel::{MsgReceiver, Payload};
#[cfg(feature = "capture")]
use api::CaptureBits;
#[cfg(feature = "replay")]
use api::CapturedDocument;
use clip_scroll_tree::{SpatialNodeIndex, ClipScrollTree};
#[cfg(feature = "debugger")]
use debug_server;
use frame_builder::{FrameBuilder, FrameBuilderConfig};
use gpu_cache::GpuCache;
use hit_test::{HitTest, HitTester};
use internal_types::{DebugOutput, FastHashMap, FastHashSet, RenderedDocument, ResultMsg};
use profiler::{BackendProfileCounters, IpcProfileCounters, ResourceProfileCounters};
use record::ApiRecordingReceiver;
use renderer::{AsyncPropertySampler, PipelineInfo};
use resource_cache::ResourceCache;
#[cfg(feature = "replay")]
use resource_cache::PlainCacheOwn;
#[cfg(any(feature = "capture", feature = "replay"))]
use resource_cache::PlainResources;
use scene::{Scene, SceneProperties};
use scene_builder::*;
#[cfg(feature = "serialize")]
use serde::{Serialize, Deserialize};
#[cfg(feature = "debugger")]
use serde_json;
#[cfg(any(feature = "capture", feature = "replay"))]
use std::path::PathBuf;
use std::sync::atomic::{ATOMIC_USIZE_INIT, AtomicUsize, Ordering};
use std::mem::replace;
use std::os::raw::c_void;
use std::sync::mpsc::{channel, Sender, Receiver};
use std::u32;
#[cfg(feature = "replay")]
use tiling::Frame;
use time::precise_time_ns;
use util::drain_filter;

#[cfg_attr(feature = "capture", derive(Serialize))]
#[cfg_attr(feature = "replay", derive(Deserialize))]
#[derive(Clone)]
pub struct DocumentView {
    pub window_size: DeviceUintSize,
    pub inner_rect: DeviceUintRect,
    pub layer: DocumentLayer,
    pub pan: DeviceIntPoint,
    pub device_pixel_ratio: f32,
    pub page_zoom_factor: f32,
    pub pinch_zoom_factor: f32,
}

impl DocumentView {
    pub fn accumulated_scale_factor(&self) -> DevicePixelScale {
        DevicePixelScale::new(
            self.device_pixel_ratio *
            self.page_zoom_factor *
            self.pinch_zoom_factor
        )
    }
}

#[derive(Copy, Clone, Hash, PartialEq, PartialOrd, Debug, Eq, Ord)]
#[cfg_attr(feature = "capture", derive(Serialize))]
#[cfg_attr(feature = "replay", derive(Deserialize))]
pub struct FrameId(pub u32);

struct Document {
    // The latest built scene, usable to build frames.
    // received from the scene builder thread.
    scene: Scene,

    // Temporary list of removed pipelines received from the scene builder
    // thread and forwarded to the renderer.
    removed_pipelines: Vec<PipelineId>,

    view: DocumentView,

    /// The ClipScrollTree for this document which tracks SpatialNodes, ClipNodes, and ClipChains.
    /// This is stored here so that we are able to preserve scrolling positions between rendered
    /// frames.
    clip_scroll_tree: ClipScrollTree,

    /// The id of the current frame.
    frame_id: FrameId,

    // the `Option` here is only to deal with borrow checker
    frame_builder: Option<FrameBuilder>,
    // A set of pipelines that the caller has requested be
    // made available as output textures.
    output_pipelines: FastHashSet<PipelineId>,

    /// A data structure to allow hit testing against rendered frames. This is updated
    /// every time we produce a fully rendered frame.
    hit_tester: Option<HitTester>,

    /// Properties that are resolved during frame building and can be changed at any time
    /// without requiring the scene to be re-built.
    dynamic_properties: SceneProperties,

    /// Track whether the last built frame is up to date or if it will need to be re-built
    /// before rendering again.
    frame_is_valid: bool,
    hit_tester_is_valid: bool,
}

impl Document {
    pub fn new(
        window_size: DeviceUintSize,
        layer: DocumentLayer,
        default_device_pixel_ratio: f32,
    ) -> Self {
        Document {
            scene: Scene::new(),
            removed_pipelines: Vec::new(),
            view: DocumentView {
                window_size,
                inner_rect: DeviceUintRect::new(DeviceUintPoint::zero(), window_size),
                layer,
                pan: DeviceIntPoint::zero(),
                page_zoom_factor: 1.0,
                pinch_zoom_factor: 1.0,
                device_pixel_ratio: default_device_pixel_ratio,
            },
            clip_scroll_tree: ClipScrollTree::new(),
            frame_id: FrameId(0),
            frame_builder: None,
            output_pipelines: FastHashSet::default(),
            hit_tester: None,
            dynamic_properties: SceneProperties::new(),
            frame_is_valid: false,
            hit_tester_is_valid: false,
        }
    }

    fn can_render(&self) -> bool {
        self.frame_builder.is_some() && self.scene.has_root_pipeline()
    }

    fn has_pixels(&self) -> bool {
        !self.view.window_size.is_empty_or_negative()
    }

    fn process_frame_msg(
        &mut self,
        message: FrameMsg,
    ) -> DocumentOps {
        match message {
            FrameMsg::UpdateEpoch(pipeline_id, epoch) => {
                self.scene.update_epoch(pipeline_id, epoch);
            }
            FrameMsg::EnableFrameOutput(pipeline_id, enable) => {
                if enable {
                    self.output_pipelines.insert(pipeline_id);
                } else {
                    self.output_pipelines.remove(&pipeline_id);
                }
            }
            FrameMsg::Scroll(delta, cursor) => {
                profile_scope!("Scroll");

                let node_index = match self.hit_tester {
                    Some(ref hit_tester) => {
                        // Ideally we would call self.scroll_nearest_scrolling_ancestor here, but
                        // we need have to avoid a double-borrow.
                        let test = HitTest::new(None, cursor, HitTestFlags::empty());
                        hit_tester.find_node_under_point(test)
                    }
                    None => {
                        None
                    }
                };

                if self.hit_tester.is_some() {
                    if self.scroll_nearest_scrolling_ancestor(delta, node_index) {
                        self.hit_tester_is_valid = false;
                        self.frame_is_valid = false;
                    }
                }

                return DocumentOps {
                    // TODO: Does it make sense to track this as a scrolling even if we
                    // ended up not scrolling anything?
                    scroll: true,
                    ..DocumentOps::nop()
                };
            }
            FrameMsg::HitTest(pipeline_id, point, flags, tx) => {

                let result = match self.hit_tester {
                    Some(ref hit_tester) => {
                        hit_tester.hit_test(HitTest::new(pipeline_id, point, flags))
                    }
                    None => HitTestResult { items: Vec::new() },
                };

                tx.send(result).unwrap();
            }
            FrameMsg::SetPan(pan) => {
                if self.view.pan != pan {
                    self.view.pan = pan;
                    self.hit_tester_is_valid = false;
                    self.frame_is_valid = false;
                }
            }
            FrameMsg::ScrollNodeWithId(origin, id, clamp) => {
                profile_scope!("ScrollNodeWithScrollId");

                if self.scroll_node(origin, id, clamp) {
                    self.hit_tester_is_valid = false;
                    self.frame_is_valid = false;
                }

                return DocumentOps {
                    scroll: true,
                    ..DocumentOps::nop()
                };
            }
            FrameMsg::GetScrollNodeState(tx) => {
                profile_scope!("GetScrollNodeState");
                tx.send(self.get_scroll_node_state()).unwrap();
            }
            FrameMsg::UpdateDynamicProperties(property_bindings) => {
                self.dynamic_properties.set_properties(property_bindings);
            }
            FrameMsg::AppendDynamicProperties(property_bindings) => {
                self.dynamic_properties.add_properties(property_bindings);
            }
        }

        DocumentOps::nop()
    }

    fn build_frame(
        &mut self,
        resource_cache: &mut ResourceCache,
        gpu_cache: &mut GpuCache,
        resource_profile: &mut ResourceProfileCounters,
        is_new_scene: bool,
    ) -> RenderedDocument {
        let accumulated_scale_factor = self.view.accumulated_scale_factor();
        let pan = self.view.pan.to_f32() / accumulated_scale_factor;

        let frame = {
            let frame_builder = self.frame_builder.as_mut().unwrap();
            let frame = frame_builder.build(
                resource_cache,
                gpu_cache,
                self.frame_id,
                &mut self.clip_scroll_tree,
                &self.scene.pipelines,
                accumulated_scale_factor,
                self.view.layer,
                pan,
                &mut resource_profile.texture_cache,
                &mut resource_profile.gpu_cache,
                &self.dynamic_properties,
            );
            self.hit_tester = Some(frame_builder.create_hit_tester(&self.clip_scroll_tree));
            frame
        };

        self.frame_is_valid = true;
        self.hit_tester_is_valid = true;

        RenderedDocument {
            frame,
            is_new_scene,
        }
    }

    pub fn updated_pipeline_info(&mut self) -> PipelineInfo {
        let removed_pipelines = replace(&mut self.removed_pipelines, Vec::new());
        PipelineInfo {
            epochs: self.scene.pipeline_epochs.clone(),
            removed_pipelines,
        }
    }

    pub fn discard_frame_state_for_pipeline(&mut self, pipeline_id: PipelineId) {
        self.clip_scroll_tree
            .discard_frame_state_for_pipeline(pipeline_id);
    }

    /// Returns true if any nodes actually changed position or false otherwise.
    pub fn scroll_nearest_scrolling_ancestor(
        &mut self,
        scroll_location: ScrollLocation,
        scroll_node_index: Option<SpatialNodeIndex>,
    ) -> bool {
        self.clip_scroll_tree.scroll_nearest_scrolling_ancestor(scroll_location, scroll_node_index)
    }

    /// Returns true if the node actually changed position or false otherwise.
    pub fn scroll_node(
        &mut self,
        origin: LayoutPoint,
        id: ExternalScrollId,
        clamp: ScrollClamping
    ) -> bool {
        self.clip_scroll_tree.scroll_node(origin, id, clamp)
    }

    pub fn get_scroll_node_state(&self) -> Vec<ScrollNodeState> {
        self.clip_scroll_tree.get_scroll_node_state()
    }

    pub fn new_async_scene_ready(&mut self, built_scene: BuiltScene) {
        self.scene = built_scene.scene;
        self.frame_is_valid = false;
        self.hit_tester_is_valid = false;

        self.frame_builder = Some(built_scene.frame_builder);

        let old_scrolling_states = self.clip_scroll_tree.drain();
        self.clip_scroll_tree = built_scene.clip_scroll_tree;
        self.clip_scroll_tree.finalize_and_apply_pending_scroll_offsets(old_scrolling_states);

        // Advance to the next frame.
        self.frame_id.0 += 1;
    }
}

struct DocumentOps {
    scroll: bool,
    build_frame: bool,
}

impl DocumentOps {
    fn nop() -> Self {
        DocumentOps {
            scroll: false,
            build_frame: false,
        }
    }
}

/// The unique id for WR resource identification.
static NEXT_NAMESPACE_ID: AtomicUsize = ATOMIC_USIZE_INIT;

#[cfg(any(feature = "capture", feature = "replay"))]
#[cfg_attr(feature = "capture", derive(Serialize))]
#[cfg_attr(feature = "replay", derive(Deserialize))]
struct PlainRenderBackend {
    default_device_pixel_ratio: f32,
    frame_config: FrameBuilderConfig,
    documents: FastHashMap<DocumentId, DocumentView>,
    resources: PlainResources,
    last_scene_id: u64,
}

/// The render backend is responsible for transforming high level display lists into
/// GPU-friendly work which is then submitted to the renderer in the form of a frame::Frame.
///
/// The render backend operates on its own thread.
pub struct RenderBackend {
    api_rx: MsgReceiver<ApiMsg>,
    payload_rx: Receiver<Payload>,
    result_tx: Sender<ResultMsg>,
    scene_tx: Sender<SceneBuilderRequest>,
    low_priority_scene_tx: Sender<SceneBuilderRequest>,
    scene_rx: Receiver<SceneBuilderResult>,

    payload_buffer: Vec<Payload>,

    default_device_pixel_ratio: f32,

    gpu_cache: GpuCache,
    resource_cache: ResourceCache,

    frame_config: FrameBuilderConfig,
    documents: FastHashMap<DocumentId, Document>,

    notifier: Box<RenderNotifier>,
    recorder: Option<Box<ApiRecordingReceiver>>,
    sampler: Option<Box<AsyncPropertySampler + Send>>,
    size_of_op: Option<VoidPtrToSizeFn>,

    last_scene_id: u64,
}

impl RenderBackend {
    pub fn new(
        api_rx: MsgReceiver<ApiMsg>,
        payload_rx: Receiver<Payload>,
        result_tx: Sender<ResultMsg>,
        scene_tx: Sender<SceneBuilderRequest>,
        low_priority_scene_tx: Sender<SceneBuilderRequest>,
        scene_rx: Receiver<SceneBuilderResult>,
        default_device_pixel_ratio: f32,
        resource_cache: ResourceCache,
        notifier: Box<RenderNotifier>,
        frame_config: FrameBuilderConfig,
        recorder: Option<Box<ApiRecordingReceiver>>,
        sampler: Option<Box<AsyncPropertySampler + Send>>,
        size_of_op: Option<VoidPtrToSizeFn>,
    ) -> RenderBackend {
        // The namespace_id should start from 1.
        NEXT_NAMESPACE_ID.fetch_add(1, Ordering::Relaxed);

        RenderBackend {
            api_rx,
            payload_rx,
            result_tx,
            scene_tx,
            low_priority_scene_tx,
            scene_rx,
            payload_buffer: Vec::new(),
            default_device_pixel_ratio,
            resource_cache,
            gpu_cache: GpuCache::new(),
            frame_config,
            documents: FastHashMap::default(),
            notifier,
            recorder,
            sampler,
            size_of_op,
            last_scene_id: 0,
        }
    }

    fn process_scene_msg(
        &mut self,
        document_id: DocumentId,
        message: SceneMsg,
        frame_counter: u32,
        txn: &mut Transaction,
        ipc_profile_counters: &mut IpcProfileCounters,
    ) {
        let doc = self.documents.get_mut(&document_id).expect("No document?");

        match message {
            SceneMsg::UpdateEpoch(pipeline_id, epoch) => {
                txn.epoch_updates.push((pipeline_id, epoch));
            }
            SceneMsg::SetPageZoom(factor) => {
                doc.view.page_zoom_factor = factor.get();
            }
            SceneMsg::SetPinchZoom(factor) => {
                doc.view.pinch_zoom_factor = factor.get();
            }
            SceneMsg::SetWindowParameters {
                window_size,
                inner_rect,
                device_pixel_ratio,
            } => {
                doc.view.window_size = window_size;
                doc.view.inner_rect = inner_rect;
                doc.view.device_pixel_ratio = device_pixel_ratio;
            }
            SceneMsg::SetDisplayList {
                epoch,
                pipeline_id,
                background,
                viewport_size,
                content_size,
                list_descriptor,
                preserve_frame_state,
            } => {
                profile_scope!("SetDisplayList");

                let data = if let Some(idx) = self.payload_buffer.iter().position(|data|
                    data.epoch == epoch && data.pipeline_id == pipeline_id
                ) {
                    self.payload_buffer.swap_remove(idx)
                } else {
                    loop {
                        let data = self.payload_rx.recv().unwrap();
                        if data.epoch == epoch && data.pipeline_id == pipeline_id {
                            break data;
                        } else {
                            self.payload_buffer.push(data);
                        }
                    }
                };

                if let Some(ref mut r) = self.recorder {
                    r.write_payload(frame_counter, &data.to_data());
                }

                let built_display_list =
                    BuiltDisplayList::from_data(data.display_list_data, list_descriptor);

                if !preserve_frame_state {
                    doc.discard_frame_state_for_pipeline(pipeline_id);
                }

                let display_list_len = built_display_list.data().len();
                let (builder_start_time, builder_finish_time, send_start_time) =
                    built_display_list.times();
                let display_list_received_time = precise_time_ns();

                txn.display_list_updates.push(DisplayListUpdate {
                    built_display_list,
                    pipeline_id,
                    epoch,
                    background,
                    viewport_size,
                    content_size,
                });

                // Note: this isn't quite right as auxiliary values will be
                // pulled out somewhere in the prim_store, but aux values are
                // really simple and cheap to access, so it's not a big deal.
                let display_list_consumed_time = precise_time_ns();

                ipc_profile_counters.set(
                    builder_start_time,
                    builder_finish_time,
                    send_start_time,
                    display_list_received_time,
                    display_list_consumed_time,
                    display_list_len,
                );
            }
            SceneMsg::SetRootPipeline(pipeline_id) => {
                profile_scope!("SetRootPipeline");

                txn.set_root_pipeline = Some(pipeline_id);
            }
            SceneMsg::RemovePipeline(pipeline_id) => {
                profile_scope!("RemovePipeline");

                txn.removed_pipelines.push(pipeline_id);
            }
        }
    }

    fn next_namespace_id(&self) -> IdNamespace {
        IdNamespace(NEXT_NAMESPACE_ID.fetch_add(1, Ordering::Relaxed) as u32)
    }

    pub fn make_unique_scene_id(&mut self) -> u64 {
        // 2^64 scenes ought to be enough for anybody!
        self.last_scene_id += 1;
        self.last_scene_id
    }

    pub fn run(&mut self, mut profile_counters: BackendProfileCounters) {
        let mut frame_counter: u32 = 0;
        let mut keep_going = true;

        if let Some(ref sampler) = self.sampler {
            sampler.register();
        }

        while keep_going {
            profile_scope!("handle_msg");

            while let Ok(msg) = self.scene_rx.try_recv() {
                match msg {
                    SceneBuilderResult::Transaction(mut txn, result_tx) => {
                        let has_built_scene = txn.built_scene.is_some();
                        if let Some(doc) = self.documents.get_mut(&txn.document_id) {

                            doc.removed_pipelines.append(&mut txn.removed_pipelines);

                            if let Some(mut built_scene) = txn.built_scene.take() {
                                doc.new_async_scene_ready(built_scene);
                            }

                            if let Some(tx) = result_tx {
                                let (resume_tx, resume_rx) = channel();
                                tx.send(SceneSwapResult::Complete(resume_tx)).unwrap();
                                // Block until the post-swap hook has completed on
                                // the scene builder thread. We need to do this before
                                // we can sample from the sampler hook which might happen
                                // in the update_document call below.
                                resume_rx.recv().ok();
                            }
                        } else {
                            // The document was removed while we were building it, skip it.
                            // TODO: we might want to just ensure that removed documents are
                            // always forwarded to the scene builder thread to avoid this case.
                            if let Some(tx) = result_tx {
                                tx.send(SceneSwapResult::Aborted).unwrap();
                            }
                            continue;
                        }

                        self.resource_cache.add_rasterized_blob_images(
                            replace(&mut txn.rasterized_blobs, Vec::new())
                        );
                        if let Some(rasterizer) = txn.blob_rasterizer.take() {
                            self.resource_cache.set_blob_rasterizer(rasterizer);
                        }

                        self.update_document(
                            txn.document_id,
                            replace(&mut txn.resource_updates, Vec::new()),
                            replace(&mut txn.frame_ops, Vec::new()),
                            replace(&mut txn.notifications, Vec::new()),
                            txn.build_frame,
                            txn.render_frame,
                            &mut frame_counter,
                            &mut profile_counters,
                            has_built_scene,
                        );
                    },
                    SceneBuilderResult::FlushComplete(tx) => {
                        tx.send(()).ok();
                    }
                    SceneBuilderResult::Stopped => {
                        panic!("We haven't sent a Stop yet, how did we get a Stopped back?");
                    }
                }
            }

            keep_going = match self.api_rx.recv() {
                Ok(msg) => {
                    if let Some(ref mut r) = self.recorder {
                        r.write_msg(frame_counter, &msg);
                    }
                    self.process_api_msg(msg, &mut profile_counters, &mut frame_counter)
                }
                Err(..) => { false }
            };
        }

        let _ = self.low_priority_scene_tx.send(SceneBuilderRequest::Stop);
        // Ensure we read everything the scene builder is sending us from
        // inflight messages, otherwise the scene builder might panic.
        while let Ok(msg) = self.scene_rx.recv() {
            match msg {
                SceneBuilderResult::FlushComplete(tx) => {
                    // If somebody's blocked waiting for a flush, how did they
                    // trigger the RB thread to shut down? This shouldn't happen
                    // but handle it gracefully anyway.
                    debug_assert!(false);
                    tx.send(()).ok();
                }
                SceneBuilderResult::Stopped => break,
                _ => continue,
            }
        }

        self.notifier.shut_down();

        if let Some(ref sampler) = self.sampler {
            sampler.deregister();
        }

    }

    fn process_api_msg(
        &mut self,
        msg: ApiMsg,
        profile_counters: &mut BackendProfileCounters,
        frame_counter: &mut u32,
    ) -> bool {
        match msg {
            ApiMsg::WakeUp => {}
            ApiMsg::WakeSceneBuilder => {
                self.scene_tx.send(SceneBuilderRequest::WakeUp).unwrap();
            }
            ApiMsg::FlushSceneBuilder(tx) => {
                self.low_priority_scene_tx.send(SceneBuilderRequest::Flush(tx)).unwrap();
            }
            ApiMsg::UpdateResources(mut updates) => {
                self.resource_cache.pre_scene_building_update(
                    &mut updates,
                    &mut profile_counters.resources
                );
                self.resource_cache.post_scene_building_update(
                    updates,
                    &mut profile_counters.resources
                );
            }
            ApiMsg::GetGlyphDimensions(instance_key, glyph_indices, tx) => {
                let mut glyph_dimensions = Vec::with_capacity(glyph_indices.len());
                if let Some(font) = self.resource_cache.get_font_instance(instance_key) {
                    for glyph_index in &glyph_indices {
                        let glyph_dim = self.resource_cache.get_glyph_dimensions(&font, *glyph_index);
                        glyph_dimensions.push(glyph_dim);
                    }
                }
                tx.send(glyph_dimensions).unwrap();
            }
            ApiMsg::GetGlyphIndices(font_key, text, tx) => {
                let mut glyph_indices = Vec::new();
                for ch in text.chars() {
                    let index = self.resource_cache.get_glyph_index(font_key, ch);
                    glyph_indices.push(index);
                }
                tx.send(glyph_indices).unwrap();
            }
            ApiMsg::CloneApi(sender) => {
                sender.send(self.next_namespace_id()).unwrap();
            }
            ApiMsg::AddDocument(document_id, initial_size, layer) => {
                let document = Document::new(
                    initial_size,
                    layer,
                    self.default_device_pixel_ratio,
                );
                self.documents.insert(document_id, document);
            }
            ApiMsg::DeleteDocument(document_id) => {
                self.documents.remove(&document_id);
                self.low_priority_scene_tx.send(
                    SceneBuilderRequest::DeleteDocument(document_id)
                ).unwrap();
            }
            ApiMsg::ExternalEvent(evt) => {
                self.notifier.external_event(evt);
            }
            ApiMsg::ClearNamespace(namespace_id) => {
                self.resource_cache.clear_namespace(namespace_id);
                self.documents.retain(|did, _doc| did.0 != namespace_id);
            }
            ApiMsg::MemoryPressure => {
                // This is drastic. It will basically flush everything out of the cache,
                // and the next frame will have to rebuild all of its resources.
                // We may want to look into something less extreme, but on the other hand this
                // should only be used in situations where are running low enough on memory
                // that we risk crashing if we don't do something about it.
                // The advantage of clearing the cache completely is that it gets rid of any
                // remaining fragmentation that could have persisted if we kept around the most
                // recently used resources.
                self.resource_cache.clear(ClearCache::all());

                let pending_update = self.resource_cache.pending_updates();
                let msg = ResultMsg::UpdateResources {
                    updates: pending_update,
                    cancel_rendering: true,
                };
                self.result_tx.send(msg).unwrap();
                self.notifier.wake_up();
            }
            ApiMsg::ReportMemory(tx) => {
                tx.send(self.report_memory()).unwrap();
            }
            ApiMsg::DebugCommand(option) => {
                let msg = match option {
                    DebugCommand::EnableDualSourceBlending(enable) => {
                        // Set in the config used for any future documents
                        // that are created.
                        self.frame_config
                            .dual_source_blending_is_enabled = enable;

                        self.low_priority_scene_tx.send(SceneBuilderRequest::SetFrameBuilderConfig(
                            self.frame_config.clone()
                        )).unwrap();

                        // We don't want to forward this message to the renderer.
                        return true;
                    }
                    DebugCommand::FetchDocuments => {
                        let json = self.get_docs_for_debugger();
                        ResultMsg::DebugOutput(DebugOutput::FetchDocuments(json))
                    }
                    DebugCommand::FetchClipScrollTree => {
                        let json = self.get_clip_scroll_tree_for_debugger();
                        ResultMsg::DebugOutput(DebugOutput::FetchClipScrollTree(json))
                    }
                    #[cfg(feature = "capture")]
                    DebugCommand::SaveCapture(root, bits) => {
                        let output = self.save_capture(root, bits, profile_counters);
                        ResultMsg::DebugOutput(output)
                    },
                    #[cfg(feature = "replay")]
                    DebugCommand::LoadCapture(root, tx) => {
                        NEXT_NAMESPACE_ID.fetch_add(1, Ordering::Relaxed);
                        *frame_counter += 1;

                        self.load_capture(&root, profile_counters);

                        for (id, doc) in &self.documents {
                            let captured = CapturedDocument {
                                document_id: *id,
                                root_pipeline_id: doc.scene.root_pipeline_id,
                                window_size: doc.view.window_size,
                            };
                            tx.send(captured).unwrap();

                            // notify the active recorder
                            if let Some(ref mut r) = self.recorder {
                                let pipeline_id = doc.scene.root_pipeline_id.unwrap();
                                let epoch =  doc.scene.pipeline_epochs[&pipeline_id];
                                let pipeline = &doc.scene.pipelines[&pipeline_id];
                                let scene_msg = SceneMsg::SetDisplayList {
                                    list_descriptor: pipeline.display_list.descriptor().clone(),
                                    epoch,
                                    pipeline_id,
                                    background: pipeline.background_color,
                                    viewport_size: pipeline.viewport_size,
                                    content_size: pipeline.content_size,
                                    preserve_frame_state: false,
                                };
                                let txn = TransactionMsg::scene_message(scene_msg);
                                r.write_msg(*frame_counter, &ApiMsg::UpdateDocument(*id, txn));
                                r.write_payload(*frame_counter, &Payload::construct_data(
                                    epoch,
                                    pipeline_id,
                                    pipeline.display_list.data(),
                                ));
                            }
                        }

                        // Note: we can't pass `LoadCapture` here since it needs to arrive
                        // before the `PublishDocument` messages sent by `load_capture`.
                        return true;
                    }
                    DebugCommand::ClearCaches(mask) => {
                        self.resource_cache.clear(mask);
                        return true;
                    }
                    _ => ResultMsg::DebugCommand(option),
                };
                self.result_tx.send(msg).unwrap();
                self.notifier.wake_up();
            }
            ApiMsg::ShutDown => {
                return false;
            }
            ApiMsg::UpdateDocument(document_id, transaction_msg) => {
                self.prepare_transaction(
                    document_id,
                    transaction_msg,
                    frame_counter,
                    profile_counters,
                );
            }
        }

        true
    }

    fn prepare_transaction(
        &mut self,
        document_id: DocumentId,
        mut transaction_msg: TransactionMsg,
        frame_counter: &mut u32,
        profile_counters: &mut BackendProfileCounters,
    ) {
        let mut txn = Box::new(Transaction {
            document_id,
            display_list_updates: Vec::new(),
            removed_pipelines: Vec::new(),
            epoch_updates: Vec::new(),
            request_scene_build: None,
            blob_rasterizer: None,
            blob_requests: Vec::new(),
            resource_updates: transaction_msg.resource_updates,
            frame_ops: transaction_msg.frame_ops,
            rasterized_blobs: Vec::new(),
            notifications: transaction_msg.notifications,
            set_root_pipeline: None,
            build_frame: transaction_msg.generate_frame,
            render_frame: transaction_msg.generate_frame,
        });

        self.resource_cache.pre_scene_building_update(
            &mut txn.resource_updates,
            &mut profile_counters.resources,
        );

        for scene_msg in transaction_msg.scene_ops.drain(..) {
            let _timer = profile_counters.total_time.timer();
            self.process_scene_msg(
                document_id,
                scene_msg,
                *frame_counter,
                &mut txn,
                &mut profile_counters.ipc,
            )
        }

        let blobs_to_rasterize = get_blob_image_updates(&txn.resource_updates);
        if !blobs_to_rasterize.is_empty() {
            let (blob_rasterizer, blob_requests) = self.resource_cache
                .create_blob_scene_builder_requests(&blobs_to_rasterize);

            txn.blob_requests = blob_requests;
            txn.blob_rasterizer = blob_rasterizer;
        }

        if !transaction_msg.use_scene_builder_thread && txn.can_skip_scene_builder() {
            self.update_document(
                txn.document_id,
                replace(&mut txn.resource_updates, Vec::new()),
                replace(&mut txn.frame_ops, Vec::new()),
                replace(&mut txn.notifications, Vec::new()),
                txn.build_frame,
                txn.render_frame,
                frame_counter,
                profile_counters,
                false
            );

            return;
        }

        let scene_id = self.make_unique_scene_id();
        let doc = self.documents.get_mut(&document_id).unwrap();

        if txn.should_build_scene() {
            txn.request_scene_build = Some(SceneRequest {
                view: doc.view.clone(),
                font_instances: self.resource_cache.get_font_instances(),
                output_pipelines: doc.output_pipelines.clone(),
                scene_id,
            });
        }

        let tx = if transaction_msg.low_priority {
            &self.low_priority_scene_tx
        } else {
            &self.scene_tx
        };

        tx.send(SceneBuilderRequest::Transaction(txn)).unwrap();
    }

    fn update_document(
        &mut self,
        document_id: DocumentId,
        resource_updates: Vec<ResourceUpdate>,
        mut frame_ops: Vec<FrameMsg>,
        mut notifications: Vec<NotificationRequest>,
        mut build_frame: bool,
        mut render_frame: bool,
        frame_counter: &mut u32,
        profile_counters: &mut BackendProfileCounters,
        has_built_scene: bool,
    ) {
        let requested_frame = render_frame;

        // If we have a sampler, get more frame ops from it and add them
        // to the transaction. This is a hook to allow the WR user code to
        // fiddle with things after a potentially long scene build, but just
        // before rendering. This is useful for rendering with the latest
        // async transforms.
        if build_frame {
            if let Some(ref sampler) = self.sampler {
                frame_ops.append(&mut sampler.sample());
            }
        }

        let doc = self.documents.get_mut(&document_id).unwrap();

        // TODO: this scroll variable doesn't necessarily mean we scrolled. It is only used
        // for something wrench specific and we should remove it.
        let mut scroll = false;
        for frame_msg in frame_ops {
            let _timer = profile_counters.total_time.timer();
            let op = doc.process_frame_msg(frame_msg);
            build_frame |= op.build_frame;
            scroll |= op.scroll;
        }

        for update in &resource_updates {
            if let ResourceUpdate::UpdateImage(..) = update {
                doc.frame_is_valid = false;
            }
        }

        self.resource_cache.post_scene_building_update(
            resource_updates,
            &mut profile_counters.resources,
        );

        // After applying the new scene we need to
        // rebuild the hit-tester, so we trigger a frame generation
        // step.
        //
        // TODO: We could avoid some the cost of building the frame by only
        // building the information required for hit-testing (See #2807).
        build_frame |= has_built_scene;

        if doc.dynamic_properties.flush_pending_updates() {
            doc.frame_is_valid = false;
            doc.hit_tester_is_valid = false;
            build_frame = true;
        }

        if !doc.can_render() {
            // TODO: this happens if we are building the first scene asynchronously and
            // scroll at the same time. we should keep track of the fact that we skipped
            // composition here and do it as soon as we receive the scene.
            build_frame = false;
            render_frame = false;
        }

        if doc.frame_is_valid {
            build_frame = false;
        }

        let mut frame_build_time = None;
        if build_frame && doc.has_pixels() {
            profile_scope!("generate frame");

            *frame_counter += 1;

            // borrow ck hack for profile_counters
            let (pending_update, rendered_document) = {
                let _timer = profile_counters.total_time.timer();
                let frame_build_start_time = precise_time_ns();

                let rendered_document = doc.build_frame(
                    &mut self.resource_cache,
                    &mut self.gpu_cache,
                    &mut profile_counters.resources,
                    has_built_scene,
                );

                debug!("generated frame for document {:?} with {} passes",
                    document_id, rendered_document.frame.passes.len());

                let msg = ResultMsg::UpdateGpuCache(self.gpu_cache.extract_updates());
                self.result_tx.send(msg).unwrap();

                frame_build_time = Some(precise_time_ns() - frame_build_start_time);

                let pending_update = self.resource_cache.pending_updates();
                (pending_update, rendered_document)
            };

            let msg = ResultMsg::PublishPipelineInfo(doc.updated_pipeline_info());
            self.result_tx.send(msg).unwrap();

            // Publish the frame
            let msg = ResultMsg::PublishDocument(
                document_id,
                rendered_document,
                pending_update,
                profile_counters.clone()
            );
            self.result_tx.send(msg).unwrap();
            profile_counters.reset();
        } else if requested_frame {
            // WR-internal optimization to avoid doing a bunch of render work if
            // there's no pixels. We still want to pretend to render and request
            // a render to make sure that the callbacks (particularly the
            // new_frame_ready callback below) has the right flags.
            let msg = ResultMsg::PublishPipelineInfo(doc.updated_pipeline_info());
            self.result_tx.send(msg).unwrap();
        }

        drain_filter(
            &mut notifications,
            |n| { n.when() == Checkpoint::FrameBuilt },
            |n| { n.notify(); },
        );

        // Always forward the transaction to the renderer if a frame was requested,
        // otherwise gecko can get into a state where it waits (forever) for the
        // transaction to complete before sending new work.
        if requested_frame {
            self.notifier.new_frame_ready(document_id, scroll, render_frame, frame_build_time);
        }
    }

    #[cfg(not(feature = "debugger"))]
    fn get_docs_for_debugger(&self) -> String {
        String::new()
    }

    #[cfg(feature = "debugger")]
    fn traverse_items<'a>(
        &self,
        traversal: &mut BuiltDisplayListIter<'a>,
        node: &mut debug_server::TreeNode,
    ) {
        loop {
            let subtraversal = {
                let item = match traversal.next() {
                    Some(item) => item,
                    None => break,
                };

                match *item.item() {
                    display_item @ SpecificDisplayItem::PushStackingContext(..) => {
                        let mut subtraversal = item.sub_iter();
                        let mut child_node =
                            debug_server::TreeNode::new(&display_item.debug_string());
                        self.traverse_items(&mut subtraversal, &mut child_node);
                        node.add_child(child_node);
                        Some(subtraversal)
                    }
                    SpecificDisplayItem::PopStackingContext => {
                        return;
                    }
                    display_item => {
                        node.add_item(&display_item.debug_string());
                        None
                    }
                }
            };

            // If flatten_item created a sub-traversal, we need `traversal` to have the
            // same state as the completed subtraversal, so we reinitialize it here.
            if let Some(subtraversal) = subtraversal {
                *traversal = subtraversal;
            }
        }
    }

    #[cfg(feature = "debugger")]
    fn get_docs_for_debugger(&self) -> String {
        let mut docs = debug_server::DocumentList::new();

        for (_, doc) in &self.documents {
            let mut debug_doc = debug_server::TreeNode::new("document");

            for (_, pipeline) in &doc.scene.pipelines {
                let mut debug_dl = debug_server::TreeNode::new("display-list");
                self.traverse_items(&mut pipeline.display_list.iter(), &mut debug_dl);
                debug_doc.add_child(debug_dl);
            }

            docs.add(debug_doc);
        }

        serde_json::to_string(&docs).unwrap()
    }

    #[cfg(not(feature = "debugger"))]
    fn get_clip_scroll_tree_for_debugger(&self) -> String {
        String::new()
    }

    #[cfg(feature = "debugger")]
    fn get_clip_scroll_tree_for_debugger(&self) -> String {
        let mut debug_root = debug_server::ClipScrollTreeList::new();

        for (_, doc) in &self.documents {
            let debug_node = debug_server::TreeNode::new("document clip-scroll tree");
            let mut builder = debug_server::TreeNodeBuilder::new(debug_node);

            doc.clip_scroll_tree.print_with(&mut builder);

            debug_root.add(builder.build());
        }

        serde_json::to_string(&debug_root).unwrap()
    }

    fn size_of<T>(&self, ptr: *const T) -> usize {
        let op = self.size_of_op.as_ref().unwrap();
        unsafe { op(ptr as *const c_void) }
    }

    fn report_memory(&self) -> MemoryReport {
        let mut report = MemoryReport::default();
        let op = self.size_of_op.as_ref().unwrap();
        report.gpu_cache_metadata = self.gpu_cache.malloc_size_of(*op);
        for (_id, doc) in &self.documents {
            if let Some(ref fb) = doc.frame_builder {
                report.primitive_stores += self.size_of(fb.prim_store.primitives.as_ptr());
                report.clip_stores += fb.clip_store.malloc_size_of(*op);
            }
            report.hit_testers +=
                doc.hit_tester.as_ref().map_or(0, |ht| ht.malloc_size_of(*op));
        }

        report
    }
}

fn get_blob_image_updates(updates: &[ResourceUpdate]) -> Vec<ImageKey> {
    let mut requests = Vec::new();
    for update in updates {
        match *update {
            ResourceUpdate::AddImage(ref img) => {
                if img.data.is_blob() {
                    requests.push(img.key);
                }
            }
            ResourceUpdate::UpdateImage(ref img) => {
                if img.data.is_blob() {
                    requests.push(img.key);
                }
            }
            _ => {}
        }
    }

    requests
}


#[cfg(feature = "debugger")]
trait ToDebugString {
    fn debug_string(&self) -> String;
}

#[cfg(feature = "debugger")]
impl ToDebugString for SpecificDisplayItem {
    fn debug_string(&self) -> String {
        match *self {
            SpecificDisplayItem::Border(..) => String::from("border"),
            SpecificDisplayItem::BoxShadow(..) => String::from("box_shadow"),
            SpecificDisplayItem::ClearRectangle => String::from("clear_rectangle"),
            SpecificDisplayItem::Clip(..) => String::from("clip"),
            SpecificDisplayItem::ClipChain(..) => String::from("clip_chain"),
            SpecificDisplayItem::Gradient(..) => String::from("gradient"),
            SpecificDisplayItem::Iframe(..) => String::from("iframe"),
            SpecificDisplayItem::Image(..) => String::from("image"),
            SpecificDisplayItem::Line(..) => String::from("line"),
            SpecificDisplayItem::PopAllShadows => String::from("pop_all_shadows"),
            SpecificDisplayItem::PopReferenceFrame => String::from("pop_reference_frame"),
            SpecificDisplayItem::PopStackingContext => String::from("pop_stacking_context"),
            SpecificDisplayItem::PushReferenceFrame(..) => String::from("push_reference_frame"),
            SpecificDisplayItem::PushShadow(..) => String::from("push_shadow"),
            SpecificDisplayItem::PushStackingContext(..) => String::from("push_stacking_context"),
            SpecificDisplayItem::RadialGradient(..) => String::from("radial_gradient"),
            SpecificDisplayItem::Rectangle(..) => String::from("rectangle"),
            SpecificDisplayItem::ScrollFrame(..) => String::from("scroll_frame"),
            SpecificDisplayItem::SetGradientStops => String::from("set_gradient_stops"),
            SpecificDisplayItem::StickyFrame(..) => String::from("sticky_frame"),
            SpecificDisplayItem::Text(..) => String::from("text"),
            SpecificDisplayItem::YuvImage(..) => String::from("yuv_image"),
        }
    }
}

impl RenderBackend {
    #[cfg(feature = "capture")]
    // Note: the mutable `self` is only needed here for resolving blob images
    fn save_capture(
        &mut self,
        root: PathBuf,
        bits: CaptureBits,
        profile_counters: &mut BackendProfileCounters,
    ) -> DebugOutput {
        use std::fs;
        use capture::CaptureConfig;

        debug!("capture: saving {:?}", root);
        if !root.is_dir() {
            if let Err(e) = fs::create_dir_all(&root) {
                panic!("Unable to create capture dir: {:?}", e);
            }
        }
        let config = CaptureConfig::new(root, bits);

        for (&id, doc) in &mut self.documents {
            debug!("\tdocument {:?}", id);
            if config.bits.contains(CaptureBits::SCENE) {
                let file_name = format!("scene-{}-{}", (id.0).0, id.1);
                config.serialize(&doc.scene, file_name);
            }
            if config.bits.contains(CaptureBits::FRAME) {
                let rendered_document = doc.build_frame(
                    &mut self.resource_cache,
                    &mut self.gpu_cache,
                    &mut profile_counters.resources,
                    true,
                );
                //TODO: write down doc's pipeline info?
                // it has `pipeline_epoch_map`,
                // which may capture necessary details for some cases.
                let file_name = format!("frame-{}-{}", (id.0).0, id.1);
                config.serialize(&rendered_document.frame, file_name);
            }
        }

        debug!("\tresource cache");
        let (resources, deferred) = self.resource_cache.save_capture(&config.root);

        info!("\tbackend");
        let backend = PlainRenderBackend {
            default_device_pixel_ratio: self.default_device_pixel_ratio,
            frame_config: self.frame_config.clone(),
            documents: self.documents
                .iter()
                .map(|(id, doc)| (*id, doc.view.clone()))
                .collect(),
            resources,
            last_scene_id: self.last_scene_id,
        };

        config.serialize(&backend, "backend");

        if config.bits.contains(CaptureBits::FRAME) {
            // After we rendered the frames, there are pending updates to both
            // GPU cache and resources. Instead of serializing them, we are going to make sure
            // they are applied on the `Renderer` side.
            let msg_update_gpu_cache = ResultMsg::UpdateGpuCache(self.gpu_cache.extract_updates());
            self.result_tx.send(msg_update_gpu_cache).unwrap();
            let msg_update_resources = ResultMsg::UpdateResources {
                updates: self.resource_cache.pending_updates(),
                cancel_rendering: false,
            };
            self.result_tx.send(msg_update_resources).unwrap();
            // Save the texture/glyph/image caches.
            info!("\tresource cache");
            let caches = self.resource_cache.save_caches(&config.root);
            config.serialize(&caches, "resource_cache");
            info!("\tgpu cache");
            config.serialize(&self.gpu_cache, "gpu_cache");
        }

        DebugOutput::SaveCapture(config, deferred)
    }

    #[cfg(feature = "replay")]
    fn load_capture(
        &mut self,
        root: &PathBuf,
        profile_counters: &mut BackendProfileCounters,
    ) {
        use capture::CaptureConfig;

        debug!("capture: loading {:?}", root);
        let backend = CaptureConfig::deserialize::<PlainRenderBackend, _>(root, "backend")
            .expect("Unable to open backend.ron");
        let caches_maybe = CaptureConfig::deserialize::<PlainCacheOwn, _>(root, "resource_cache");

        // Note: it would be great to have `RenderBackend` to be split
        // rather explicitly on what's used before and after scene building
        // so that, for example, we never miss anything in the code below:

        let plain_externals = self.resource_cache.load_capture(backend.resources, caches_maybe, root);
        let msg_load = ResultMsg::DebugOutput(
            DebugOutput::LoadCapture(root.clone(), plain_externals)
        );
        self.result_tx.send(msg_load).unwrap();

        self.gpu_cache = match CaptureConfig::deserialize::<GpuCache, _>(root, "gpu_cache") {
            Some(gpu_cache) => gpu_cache,
            None => GpuCache::new(),
        };

        self.documents.clear();
        self.default_device_pixel_ratio = backend.default_device_pixel_ratio;
        self.frame_config = backend.frame_config;

        let mut scenes_to_build = Vec::new();

        let mut last_scene_id = backend.last_scene_id;
        for (id, view) in backend.documents {
            debug!("\tdocument {:?}", id);
            let scene_name = format!("scene-{}-{}", (id.0).0, id.1);
            let scene = CaptureConfig::deserialize::<Scene, _>(root, &scene_name)
                .expect(&format!("Unable to open {}.ron", scene_name));

            let mut doc = Document {
                scene: scene.clone(),
                removed_pipelines: Vec::new(),
                view: view.clone(),
                clip_scroll_tree: ClipScrollTree::new(),
                frame_id: FrameId(0),
                frame_builder: Some(FrameBuilder::empty()),
                output_pipelines: FastHashSet::default(),
                dynamic_properties: SceneProperties::new(),
                hit_tester: None,
                frame_is_valid: false,
                hit_tester_is_valid: false,
            };

            let frame_name = format!("frame-{}-{}", (id.0).0, id.1);
            let frame = CaptureConfig::deserialize::<Frame, _>(root, frame_name);
            let build_frame = match frame {
                Some(frame) => {
                    info!("\tloaded a built frame with {} passes", frame.passes.len());

                    let msg_update = ResultMsg::UpdateGpuCache(self.gpu_cache.extract_updates());
                    self.result_tx.send(msg_update).unwrap();

                    let msg_publish = ResultMsg::PublishDocument(
                        id,
                        RenderedDocument { frame, is_new_scene: true },
                        self.resource_cache.pending_updates(),
                        profile_counters.clone(),
                    );
                    self.result_tx.send(msg_publish).unwrap();
                    profile_counters.reset();

                    self.notifier.new_frame_ready(id, false, true, None);

                    // We deserialized the state of the frame so we don't want to build
                    // it (but we do want to update the scene builder's state)
                    false
                }
                None => true,
            };

            last_scene_id += 1;

            scenes_to_build.push(LoadScene {
                document_id: id,
                scene: doc.scene.clone(),
                view: view.clone(),
                config: self.frame_config.clone(),
                output_pipelines: doc.output_pipelines.clone(),
                font_instances: self.resource_cache.get_font_instances(),
                scene_id: last_scene_id,
                build_frame,
            });

            self.documents.insert(id, doc);
        }

        if !scenes_to_build.is_empty() {
            self.low_priority_scene_tx.send(
                SceneBuilderRequest::LoadScenes(scenes_to_build)
            ).unwrap();
        }
    }
}

