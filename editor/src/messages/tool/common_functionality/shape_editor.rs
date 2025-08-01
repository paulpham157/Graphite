use super::graph_modification_utils::merge_layers;
use super::snapping::{SnapCache, SnapCandidatePoint, SnapData, SnapManager, SnappedPoint};
use super::utility_functions::{adjust_handle_colinearity, calculate_bezier_bbox, calculate_segment_angle, restore_g1_continuity, restore_previous_handle_position};
use crate::consts::HANDLE_LENGTH_FACTOR;
use crate::messages::portfolio::document::overlays::utility_functions::selected_segments;
use crate::messages::portfolio::document::utility_types::document_metadata::{DocumentMetadata, LayerNodeIdentifier};
use crate::messages::portfolio::document::utility_types::misc::{PathSnapSource, SnapSource};
use crate::messages::portfolio::document::utility_types::network_interface::NodeNetworkInterface;
use crate::messages::preferences::SelectionMode;
use crate::messages::prelude::*;
use crate::messages::tool::common_functionality::snapping::SnapTypeConfiguration;
use crate::messages::tool::common_functionality::utility_functions::{is_intersecting, is_visible_point};
use crate::messages::tool::tool_messages::path_tool::{PathOverlayMode, PointSelectState};
use bezier_rs::{Bezier, BezierHandles, Subpath, TValue};
use glam::{DAffine2, DVec2};
use graphene_std::vector::{HandleExt, HandleId, SegmentId};
use graphene_std::vector::{ManipulatorPointId, PointId, VectorData, VectorModificationType};
use std::f64::consts::TAU;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SelectionChange {
	Clear,
	Extend,
	Shrink,
}

#[derive(Clone, Copy, Debug)]
pub enum SelectionShape<'a> {
	Box([DVec2; 2]),
	Lasso(&'a Vec<DVec2>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SelectionShapeType {
	Box,
	Lasso,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone, Default)]
pub enum ManipulatorAngle {
	#[default]
	Colinear,
	Free,
	Mixed,
}

#[derive(Clone, Debug, Default)]
pub struct SelectedLayerState {
	selected_points: HashSet<ManipulatorPointId>,
	selected_segments: HashSet<SegmentId>,
	/// Keeps track of the current state; helps avoid unnecessary computation when called by [`ShapeState`].
	ignore_handles: bool,
	ignore_anchors: bool,
	/// Points that are selected but ignored (when their overlays are disabled) are stored here.
	ignored_handle_points: HashSet<ManipulatorPointId>,
	ignored_anchor_points: HashSet<ManipulatorPointId>,
}

impl SelectedLayerState {
	pub fn selected_points(&self) -> impl Iterator<Item = ManipulatorPointId> + '_ {
		self.selected_points.iter().copied()
	}

	pub fn selected_segments(&self) -> impl Iterator<Item = SegmentId> + '_ {
		self.selected_segments.iter().copied()
	}

	pub fn selected_points_count(&self) -> usize {
		self.selected_points.len()
	}

	pub fn selected_segments_count(&self) -> usize {
		self.selected_segments.len()
	}

	pub fn is_segment_selected(&self, segment: SegmentId) -> bool {
		self.selected_segments.contains(&segment)
	}

	pub fn is_point_selected(&self, point: ManipulatorPointId) -> bool {
		self.selected_points.contains(&point)
	}

	pub fn select_point(&mut self, point: ManipulatorPointId) {
		self.selected_points.insert(point);
	}

	pub fn select_segment(&mut self, segment: SegmentId) {
		self.selected_segments.insert(segment);
	}

	pub fn deselect_point(&mut self, point: ManipulatorPointId) {
		self.selected_points.remove(&point);
	}

	pub fn deselect_segment(&mut self, segment: SegmentId) {
		self.selected_segments.remove(&segment);
	}

	pub fn deselect_all_points_in_layer(&mut self) {
		self.selected_points.clear();
	}

	pub fn deselect_all_segments_in_layer(&mut self) {
		self.selected_segments.clear();
	}

	pub fn clear_points(&mut self) {
		self.selected_points.clear();
	}

	pub fn clear_segments(&mut self) {
		self.selected_segments.clear();
	}

	pub fn ignore_handles(&mut self, status: bool) {
		if self.ignore_handles != status {
			return;
		}

		self.ignore_handles = !status;

		if self.ignore_handles {
			self.ignored_handle_points.extend(self.selected_points.iter().copied().filter(|point| point.as_handle().is_some()));
			self.selected_points.retain(|point| !self.ignored_handle_points.contains(point));
		} else {
			self.selected_points.extend(self.ignored_handle_points.iter().copied());
			self.ignored_handle_points.clear();
		}
	}

	pub fn ignore_anchors(&mut self, status: bool) {
		if self.ignore_anchors != status {
			return;
		}

		self.ignore_anchors = !status;

		if self.ignore_anchors {
			self.ignored_anchor_points.extend(self.selected_points.iter().copied().filter(|point| point.as_anchor().is_some()));
			self.selected_points.retain(|point| !self.ignored_anchor_points.contains(point));
		} else {
			self.selected_points.extend(self.ignored_anchor_points.iter().copied());
			self.ignored_anchor_points.clear();
		}
	}
}

pub type SelectedShapeState = HashMap<LayerNodeIdentifier, SelectedLayerState>;

#[derive(Debug, Default)]
pub struct ShapeState {
	/// The layers we can select and edit manipulators (anchors and handles) from.
	pub selected_shape_state: SelectedShapeState,
	ignore_handles: bool,
	ignore_anchors: bool,
}

#[derive(Debug)]
pub struct SelectedPointsInfo {
	pub points: Vec<ManipulatorPointInfo>,
	pub offset: DVec2,
	pub vector_data: VectorData,
}

#[derive(Debug)]
pub struct SelectedSegmentsInfo {
	pub segments: Vec<SegmentId>,
	pub vector_data: VectorData,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManipulatorPointInfo {
	pub layer: LayerNodeIdentifier,
	pub point_id: ManipulatorPointId,
}

pub type OpposingHandleLengths = HashMap<LayerNodeIdentifier, HashMap<HandleId, f64>>;

#[derive(Clone)]
pub struct ClosestSegment {
	layer: LayerNodeIdentifier,
	segment: SegmentId,
	bezier: Bezier,
	points: [PointId; 2],
	colinear: [Option<HandleId>; 2],
	t: f64,
	bezier_point_to_viewport: DVec2,
}

impl ClosestSegment {
	pub fn layer(&self) -> LayerNodeIdentifier {
		self.layer
	}

	pub fn segment(&self) -> SegmentId {
		self.segment
	}

	pub fn points(&self) -> [PointId; 2] {
		self.points
	}

	pub fn bezier(&self) -> Bezier {
		self.bezier
	}

	pub fn closest_point_document(&self) -> DVec2 {
		self.bezier.evaluate(TValue::Parametric(self.t))
	}

	pub fn closest_point_to_viewport(&self) -> DVec2 {
		self.bezier_point_to_viewport
	}

	pub fn closest_point(&self, document_metadata: &DocumentMetadata, network_interface: &NodeNetworkInterface) -> DVec2 {
		let transform = document_metadata.transform_to_viewport_if_feeds(self.layer, network_interface);
		let bezier_point = self.bezier.evaluate(TValue::Parametric(self.t));
		transform.transform_point2(bezier_point)
	}

	/// Updates this [`ClosestSegment`] with the viewport-space location of the closest point on the segment to the given mouse position.
	pub fn update_closest_point(&mut self, document_metadata: &DocumentMetadata, network_interface: &NodeNetworkInterface, mouse_position: DVec2) {
		let transform = document_metadata.transform_to_viewport_if_feeds(self.layer, network_interface);
		let layer_mouse_pos = transform.inverse().transform_point2(mouse_position);

		let t = self.bezier.project(layer_mouse_pos).clamp(0., 1.);
		self.t = t;

		let bezier_point = self.bezier.evaluate(TValue::Parametric(t));
		let bezier_point = transform.transform_point2(bezier_point);
		self.bezier_point_to_viewport = bezier_point;
	}

	pub fn distance_squared(&self, mouse_position: DVec2) -> f64 {
		self.bezier_point_to_viewport.distance_squared(mouse_position)
	}

	pub fn too_far(&self, mouse_position: DVec2, tolerance: f64) -> bool {
		tolerance.powi(2) < self.distance_squared(mouse_position)
	}

	pub fn handle_positions(&self, document_metadata: &DocumentMetadata, network_interface: &NodeNetworkInterface) -> (Option<DVec2>, Option<DVec2>) {
		// Transform to viewport space
		let transform = document_metadata.transform_to_viewport_if_feeds(self.layer, network_interface);

		// Split the Bezier at the parameter `t`
		let [first, second] = self.bezier.split(TValue::Parametric(self.t));

		// Transform the handle positions to viewport space
		let first_handle = first.handle_end().map(|handle| transform.transform_point2(handle));
		let second_handle = second.handle_start().map(|handle| transform.transform_point2(handle));

		(first_handle, second_handle)
	}

	pub fn adjusted_insert(&self, responses: &mut VecDeque<Message>) -> (PointId, [SegmentId; 2]) {
		let layer = self.layer;
		let [first, second] = self.bezier.split(TValue::Parametric(self.t));

		// Point
		let midpoint = PointId::generate();
		let modification_type = VectorModificationType::InsertPoint { id: midpoint, position: first.end };
		responses.add(GraphOperationMessage::Vector { layer, modification_type });

		// First segment
		let segment_ids = [SegmentId::generate(), SegmentId::generate()];
		let modification_type = VectorModificationType::InsertSegment {
			id: segment_ids[0],
			points: [self.points[0], midpoint],
			handles: [first.handle_start().map(|handle| handle - first.start), first.handle_end().map(|handle| handle - first.end)],
		};
		responses.add(GraphOperationMessage::Vector { layer, modification_type });

		// Last segment
		let modification_type = VectorModificationType::InsertSegment {
			id: segment_ids[1],
			points: [midpoint, self.points[1]],
			handles: [second.handle_start().map(|handle| handle - second.start), second.handle_end().map(|handle| handle - second.end)],
		};
		responses.add(GraphOperationMessage::Vector { layer, modification_type });

		// G1 continuous on new handles
		if self.bezier.handle_end().is_some() {
			let handles = [HandleId::end(segment_ids[0]), HandleId::primary(segment_ids[1])];
			let modification_type = VectorModificationType::SetG1Continuous { handles, enabled: true };
			responses.add(GraphOperationMessage::Vector { layer, modification_type });
		}

		// Remove old segment
		let modification_type = VectorModificationType::RemoveSegment { id: self.segment };
		responses.add(GraphOperationMessage::Vector { layer, modification_type });

		// Restore mirroring on end handles
		for (handle, other) in self.colinear.into_iter().zip([HandleId::primary(segment_ids[0]), HandleId::end(segment_ids[1])]) {
			let Some(handle) = handle else { continue };
			let handles = [handle, other];
			let modification_type = VectorModificationType::SetG1Continuous { handles, enabled: true };
			responses.add(GraphOperationMessage::Vector { layer, modification_type });
		}

		(midpoint, segment_ids)
	}

	pub fn adjusted_insert_and_select(&self, shape_editor: &mut ShapeState, responses: &mut VecDeque<Message>, extend_selection: bool) {
		let (id, _) = self.adjusted_insert(responses);
		shape_editor.select_anchor_point_by_id(self.layer, id, extend_selection)
	}

	pub fn calculate_perp(&self, document: &DocumentMessageHandler) -> DVec2 {
		let tangent = if let (Some(handle1), Some(handle2)) = self.handle_positions(document.metadata(), &document.network_interface) {
			(handle1 - handle2).try_normalize()
		} else {
			let [first_point, last_point] = self.points();
			if let Some(vector_data) = document.network_interface.compute_modified_vector(self.layer()) {
				if let (Some(pos1), Some(pos2)) = (
					ManipulatorPointId::Anchor(first_point).get_position(&vector_data),
					ManipulatorPointId::Anchor(last_point).get_position(&vector_data),
				) {
					(pos1 - pos2).try_normalize()
				} else {
					None
				}
			} else {
				None
			}
		}
		.unwrap_or(DVec2::ZERO);
		tangent.perp()
	}

	/// Molding the bezier curve.
	/// Returns adjacent handles' [`HandleId`] if colinearity is broken temporarily.
	pub fn mold_handle_positions(
		&self,
		document: &DocumentMessageHandler,
		responses: &mut VecDeque<Message>,
		(c1, c2): (DVec2, DVec2),
		new_b: DVec2,
		break_colinear_molding: bool,
		temporary_adjacent_handles_while_molding: Option<[Option<HandleId>; 2]>,
	) -> Option<[Option<HandleId>; 2]> {
		let transform = document.metadata().transform_to_viewport_if_feeds(self.layer, &document.network_interface);

		let start = self.bezier.start;
		let end = self.bezier.end;

		// Apply the drag delta to the segment's handles
		let b = self.bezier_point_to_viewport;
		let delta = transform.inverse().transform_vector2(new_b - b);
		let (nc1, nc2) = (c1 + delta, c2 + delta);

		let handle1 = HandleId::primary(self.segment);
		let handle2 = HandleId::end(self.segment);
		let layer = self.layer;

		let modification_type = handle1.set_relative_position(nc1 - start);
		responses.add(GraphOperationMessage::Vector { layer, modification_type });

		let modification_type = handle2.set_relative_position(nc2 - end);
		responses.add(GraphOperationMessage::Vector { layer, modification_type });

		// If adjacent segments have colinear handles, their direction is changed but their handle lengths is preserved
		// TODO: Find something which is more appropriate
		let vector_data = document.network_interface.compute_modified_vector(self.layer())?;

		if break_colinear_molding {
			// Disable G1 continuity
			let other_handles = [
				restore_previous_handle_position(handle1, c1, start, &vector_data, layer, responses),
				restore_previous_handle_position(handle2, c2, end, &vector_data, layer, responses),
			];

			// Store other HandleId in tool data to regain colinearity later
			if temporary_adjacent_handles_while_molding.is_some() {
				temporary_adjacent_handles_while_molding
			} else {
				Some(other_handles)
			}
		} else {
			// Move the colinear handles so that colinearity is maintained
			adjust_handle_colinearity(handle1, start, nc1, &vector_data, layer, responses);
			adjust_handle_colinearity(handle2, end, nc2, &vector_data, layer, responses);

			if let Some(adjacent_handles) = temporary_adjacent_handles_while_molding {
				if let Some(other_handle1) = adjacent_handles[0] {
					restore_g1_continuity(handle1, other_handle1, nc1, start, &vector_data, layer, responses);
				}
				if let Some(other_handle2) = adjacent_handles[1] {
					restore_g1_continuity(handle2, other_handle2, nc2, end, &vector_data, layer, responses);
				}
			}
			None
		}
	}
}

// TODO Consider keeping a list of selected manipulators to minimize traversals of the layers
impl ShapeState {
	pub fn is_selected_layer(&self, layer: LayerNodeIdentifier) -> bool {
		self.selected_shape_state.contains_key(&layer)
	}

	pub fn is_point_ignored(&self, point: &ManipulatorPointId) -> bool {
		(point.as_handle().is_some() && self.ignore_handles) || (point.as_anchor().is_some() && self.ignore_anchors)
	}

	pub fn close_selected_path(&self, document: &DocumentMessageHandler, responses: &mut VecDeque<Message>) {
		// First collect all selected anchor points across all layers
		let all_selected_points: Vec<(LayerNodeIdentifier, PointId)> = self
			.selected_shape_state
			.iter()
			.flat_map(|(&layer, state)| {
				if document.network_interface.compute_modified_vector(layer).is_none() {
					return Vec::new().into_iter();
				};

				// Collect selected anchor points from this layer
				state
					.selected_points
					.iter()
					.filter_map(|&point| if let ManipulatorPointId::Anchor(id) = point { Some((layer, id)) } else { None })
					.collect::<Vec<_>>()
					.into_iter()
			})
			.collect();

		// If exactly two points are selected (regardless of layer), connect them
		if all_selected_points.len() == 2 {
			let (layer1, start_point) = all_selected_points[0];
			let (layer2, end_point) = all_selected_points[1];

			let Some(vector_data1) = document.network_interface.compute_modified_vector(layer1) else { return };
			let Some(vector_data2) = document.network_interface.compute_modified_vector(layer2) else { return };

			if vector_data1.all_connected(start_point).count() != 1 || vector_data2.all_connected(end_point).count() != 1 {
				return;
			}

			if layer1 == layer2 {
				if start_point == end_point {
					return;
				}

				let segment_id = SegmentId::generate();
				let modification_type = VectorModificationType::InsertSegment {
					id: segment_id,
					points: [end_point, start_point],
					handles: [None, None],
				};
				responses.add(GraphOperationMessage::Vector { layer: layer1, modification_type });
			} else {
				// Merge the layers
				merge_layers(document, layer1, layer2, responses);
				// Create segment between the two points
				let segment_id = SegmentId::generate();
				let modification_type = VectorModificationType::InsertSegment {
					id: segment_id,
					points: [end_point, start_point],
					handles: [None, None],
				};
				responses.add(GraphOperationMessage::Vector { layer: layer1, modification_type });
			}
			return;
		}

		// If no points are selected, try to find a single continuous subpath in each layer to connect the endpoints of
		for &layer in self.selected_shape_state.keys() {
			let Some(vector_data) = document.network_interface.compute_modified_vector(layer) else { continue };

			let endpoints: Vec<PointId> = vector_data
				.point_domain
				.ids()
				.iter()
				.copied()
				.filter(|&point_id| vector_data.all_connected(point_id).count() == 1)
				.collect();

			if endpoints.len() == 2 {
				let start_point = endpoints[0];
				let end_point = endpoints[1];

				let segment_id = SegmentId::generate();
				let modification_type = VectorModificationType::InsertSegment {
					id: segment_id,
					points: [end_point, start_point],
					handles: [None, None],
				};
				responses.add(GraphOperationMessage::Vector { layer, modification_type });
			}
		}
	}

	// Snap, returning a viewport delta
	pub fn snap(&self, snap_manager: &mut SnapManager, snap_cache: &SnapCache, document: &DocumentMessageHandler, input: &InputPreprocessorMessageHandler, previous_mouse: DVec2) -> DVec2 {
		let snap_data = SnapData::new_snap_cache(document, input, snap_cache);

		let mouse_delta = document
			.network_interface
			.document_metadata()
			.document_to_viewport
			.inverse()
			.transform_vector2(input.mouse.position - previous_mouse);
		let mut offset = mouse_delta;
		let mut best_snapped = SnappedPoint::infinite_snap(document.metadata().document_to_viewport.inverse().transform_point2(input.mouse.position));
		for (layer, state) in &self.selected_shape_state {
			let Some(vector_data) = document.network_interface.compute_modified_vector(*layer) else {
				continue;
			};

			let to_document = document.metadata().transform_to_document_if_feeds(*layer, &document.network_interface);

			for &selected in &state.selected_points {
				let source = match selected {
					ManipulatorPointId::Anchor(_) if vector_data.colinear(selected) => SnapSource::Path(PathSnapSource::AnchorPointWithColinearHandles),
					ManipulatorPointId::Anchor(_) => SnapSource::Path(PathSnapSource::AnchorPointWithFreeHandles),
					// TODO: This doesn't actually work for handles, instead handles enter the arm above for free handles
					ManipulatorPointId::PrimaryHandle(_) | ManipulatorPointId::EndHandle(_) => SnapSource::Path(PathSnapSource::HandlePoint),
				};

				let Some(position) = selected.get_position(&vector_data) else { continue };
				let mut point = SnapCandidatePoint::new_source(to_document.transform_point2(position) + mouse_delta, source);

				if let Some(id) = selected.as_anchor() {
					for neighbor in vector_data.connected_points(id) {
						if state.is_point_selected(ManipulatorPointId::Anchor(neighbor)) {
							continue;
						}
						let Some(position) = vector_data.point_domain.position_from_id(neighbor) else { continue };
						point.neighbors.push(to_document.transform_point2(position));
					}
				}

				let snapped = snap_manager.free_snap(&snap_data, &point, SnapTypeConfiguration::default());
				if best_snapped.other_snap_better(&snapped) {
					offset = snapped.snapped_point_document - point.document_point + mouse_delta;
					best_snapped = snapped;
				}
			}
		}
		snap_manager.update_indicator(best_snapped);
		document.metadata().document_to_viewport.transform_vector2(offset)
	}

	/// Select/deselect the first point within the selection threshold.
	/// Returns a tuple of the points if found and the offset, or `None` otherwise.
	pub fn change_point_selection(
		&mut self,
		network_interface: &NodeNetworkInterface,
		mouse_position: DVec2,
		select_threshold: f64,
		extend_selection: bool,
		path_overlay_mode: PathOverlayMode,
		frontier_handles_info: Option<HashMap<SegmentId, Vec<PointId>>>,
	) -> Option<Option<SelectedPointsInfo>> {
		if self.selected_shape_state.is_empty() {
			return None;
		}

		if let Some((layer, manipulator_point_id)) = self.find_nearest_visible_point_indices(network_interface, mouse_position, select_threshold, path_overlay_mode, frontier_handles_info) {
			let vector_data = network_interface.compute_modified_vector(layer)?;
			let point_position = manipulator_point_id.get_position(&vector_data)?;

			let selected_shape_state = self.selected_shape_state.get(&layer)?;
			let already_selected = selected_shape_state.is_point_selected(manipulator_point_id);

			// Offset to snap the selected point to the cursor
			let offset = mouse_position
				- network_interface
					.document_metadata()
					.transform_to_viewport_if_feeds(layer, network_interface)
					.transform_point2(point_position);

			// This is selecting the manipulator only for now, next to generalize to points

			let retain_existing_selection = extend_selection || already_selected;
			if !retain_existing_selection {
				self.deselect_all_points();
				self.deselect_all_segments();
			}

			// Add to the selected points (deselect is managed in DraggingState, DragStop)
			let selected_shape_state = self.selected_shape_state.get_mut(&layer)?;
			selected_shape_state.select_point(manipulator_point_id);

			let points = self
				.selected_shape_state
				.iter()
				.flat_map(|(layer, state)| state.selected_points.iter().map(|&point_id| ManipulatorPointInfo { layer: *layer, point_id }))
				.collect();

			return Some(Some(SelectedPointsInfo { points, offset, vector_data }));
		}
		None
	}

	pub fn get_point_selection_state(
		&mut self,
		network_interface: &NodeNetworkInterface,
		mouse_position: DVec2,
		select_threshold: f64,
		path_overlay_mode: PathOverlayMode,
		frontier_handles_info: Option<HashMap<SegmentId, Vec<PointId>>>,
		point_editing_mode: bool,
	) -> Option<(bool, Option<SelectedPointsInfo>)> {
		if self.selected_shape_state.is_empty() {
			return None;
		}

		if !point_editing_mode {
			return None;
		}

		if let Some((layer, manipulator_point_id)) = self.find_nearest_point_indices(network_interface, mouse_position, select_threshold) {
			let vector_data = network_interface.compute_modified_vector(layer)?;
			let point_position = manipulator_point_id.get_position(&vector_data)?;

			// Check if point is visible under current overlay mode or not
			let selected_segments = selected_segments(network_interface, self);
			let selected_points = self.selected_points().cloned().collect::<HashSet<_>>();
			if !is_visible_point(manipulator_point_id, &vector_data, path_overlay_mode, frontier_handles_info, selected_segments, &selected_points) {
				return None;
			}

			let selected_shape_state = self.selected_shape_state.get(&layer)?;
			let already_selected = selected_shape_state.is_point_selected(manipulator_point_id);

			// Offset to snap the selected point to the cursor
			let offset = mouse_position
				- network_interface
					.document_metadata()
					.transform_to_viewport_if_feeds(layer, network_interface)
					.transform_point2(point_position);

			// Gather current selection information
			let points = self
				.selected_shape_state
				.iter()
				.flat_map(|(layer, state)| state.selected_points.iter().map(|&point_id| ManipulatorPointInfo { layer: *layer, point_id }))
				.collect();

			let selection_info = SelectedPointsInfo { points, offset, vector_data };

			// Return the current selection state and info
			return Some((already_selected, Some(selection_info)));
		}

		None
	}

	pub fn select_anchor_point_by_id(&mut self, layer: LayerNodeIdentifier, id: PointId, extend_selection: bool) {
		if !extend_selection {
			self.deselect_all_points();
		}
		let point = ManipulatorPointId::Anchor(id);
		let Some(selected_state) = self.selected_shape_state.get_mut(&layer) else { return };
		selected_state.select_point(point);
	}

	/// Selects all anchors connected to the selected subpath, and deselects all handles, for the given layer.
	pub fn select_connected(&mut self, document: &DocumentMessageHandler, layer: LayerNodeIdentifier, mouse: DVec2, points: bool, segments: bool) {
		let Some(vector_data) = document.network_interface.compute_modified_vector(layer) else {
			return;
		};
		let to_viewport = document.metadata().transform_to_viewport_if_feeds(layer, &document.network_interface);
		let layer_mouse = to_viewport.inverse().transform_point2(mouse);
		let state = self.selected_shape_state.entry(layer).or_default();

		let mut selected_stack = Vec::new();
		// Find all subpaths that have been clicked
		for stroke in vector_data.stroke_bezier_paths() {
			if stroke.contains_point(layer_mouse) {
				if let Some(first) = stroke.manipulator_groups().first() {
					selected_stack.push(first.id);
				}
			}
		}
		state.clear_points();

		if selected_stack.is_empty() {
			// Fall back on just selecting all points/segments in the layer
			if points {
				for &point in vector_data.point_domain.ids() {
					state.select_point(ManipulatorPointId::Anchor(point));
				}
			}
			if segments {
				for &segment in vector_data.segment_domain.ids() {
					state.select_segment(segment);
				}
			}
			return;
		}

		let mut connected_points = HashSet::new();

		while let Some(point) = selected_stack.pop() {
			if !connected_points.contains(&point) {
				connected_points.insert(point);
				selected_stack.extend(vector_data.connected_points(point));
			}
		}

		if points {
			connected_points.iter().for_each(|point| state.select_point(ManipulatorPointId::Anchor(*point)));
		}

		if segments {
			for (id, _, start, end) in vector_data.segment_bezier_iter() {
				if connected_points.contains(&start) || connected_points.contains(&end) {
					state.select_segment(id);
				}
			}
		}
	}

	/// Selects all anchors, and deselects all handles, for the given layer.
	pub fn select_all_anchors_in_layer(&mut self, document: &DocumentMessageHandler, layer: LayerNodeIdentifier) {
		let state = self.selected_shape_state.entry(layer).or_default();
		Self::select_all_anchors_in_layer_with_state(document, layer, state);
	}

	/// Selects all anchors, and deselects all handles, for the selected layers.
	pub fn select_all_anchors_in_selected_layers(&mut self, document: &DocumentMessageHandler) {
		for (&layer, state) in self.selected_shape_state.iter_mut() {
			Self::select_all_anchors_in_layer_with_state(document, layer, state);
		}
	}

	/// Internal helper function that selects all anchors, and deselects all handles, for a layer given its [`LayerNodeIdentifier`] and [`SelectedLayerState`].
	fn select_all_anchors_in_layer_with_state(document: &DocumentMessageHandler, layer: LayerNodeIdentifier, state: &mut SelectedLayerState) {
		let Some(vector_data) = document.network_interface.compute_modified_vector(layer) else {
			return;
		};

		state.clear_points();

		for &point in vector_data.point_domain.ids() {
			state.select_point(ManipulatorPointId::Anchor(point))
		}
	}

	/// Deselects all points (anchors and handles) across every selected layer.
	pub fn deselect_all_points(&mut self) {
		for state in self.selected_shape_state.values_mut() {
			state.selected_points.clear()
		}
	}

	/// Deselects all segments across every selected layer
	pub fn deselect_all_segments(&mut self) {
		for state in self.selected_shape_state.values_mut() {
			state.selected_segments.clear()
		}
	}

	pub fn update_selected_anchors_status(&mut self, status: bool) {
		for state in self.selected_shape_state.values_mut() {
			self.ignore_anchors = !status;
			state.ignore_anchors(status);
		}
	}

	pub fn update_selected_handles_status(&mut self, status: bool) {
		for state in self.selected_shape_state.values_mut() {
			self.ignore_handles = !status;
			state.ignore_handles(status);
		}
	}

	/// Deselects all the anchors across every selected layer.
	pub fn deselect_all_anchors(&mut self) {
		for (_, state) in self.selected_shape_state.iter_mut() {
			let selected_anchor_points: Vec<ManipulatorPointId> = state.selected_points.iter().filter(|selected_point| selected_point.as_anchor().is_some()).cloned().collect();

			for point in selected_anchor_points {
				state.deselect_point(point);
			}
		}
	}

	/// Deselects all the handles across every selected layer.
	pub fn deselect_all_handles(&mut self) {
		for (_, state) in self.selected_shape_state.iter_mut() {
			let selected_handle_points: Vec<ManipulatorPointId> = state.selected_points.iter().filter(|selected_point| selected_point.as_handle().is_some()).cloned().collect();

			for point in selected_handle_points {
				state.deselect_point(point);
			}
		}
	}

	/// Set the shapes we consider for selection, we will choose draggable manipulators from these shapes.
	pub fn set_selected_layers(&mut self, target_layers: Vec<LayerNodeIdentifier>) {
		self.selected_shape_state.retain(|layer_path, _| target_layers.contains(layer_path));
		for layer in target_layers {
			self.selected_shape_state.entry(layer).or_default();
		}
	}

	/// Returns an iterator over the currently selected layers to get their [`LayerNodeIdentifier`]s.
	pub fn selected_layers(&self) -> impl Iterator<Item = &LayerNodeIdentifier> {
		self.selected_shape_state.keys()
	}

	/// iterate over all selected layers in order from top to bottom
	/// # WARN
	/// iterate over all layers of the document
	pub fn sorted_selected_layers<'a>(&'a self, document_metadata: &'a DocumentMetadata) -> impl Iterator<Item = LayerNodeIdentifier> + 'a {
		document_metadata.all_layers().filter(|layer| self.selected_shape_state.contains_key(layer))
	}

	pub fn has_selected_layers(&self) -> bool {
		!self.selected_shape_state.is_empty()
	}

	/// Provide the currently selected points by reference.
	pub fn selected_points(&self) -> impl Iterator<Item = &'_ ManipulatorPointId> {
		self.selected_shape_state.values().flat_map(|state| &state.selected_points)
	}

	pub fn selected_segments(&self) -> impl Iterator<Item = &'_ SegmentId> {
		self.selected_shape_state.values().flat_map(|state| &state.selected_segments)
	}

	pub fn selected_points_in_layer(&self, layer: LayerNodeIdentifier) -> Option<&HashSet<ManipulatorPointId>> {
		self.selected_shape_state.get(&layer).map(|state| &state.selected_points)
	}

	pub fn selected_segments_in_layer(&self, layer: LayerNodeIdentifier) -> Option<&HashSet<SegmentId>> {
		self.selected_shape_state.get(&layer).map(|state| &state.selected_segments)
	}

	pub fn move_primary(&self, segment: SegmentId, delta: DVec2, layer: LayerNodeIdentifier, responses: &mut VecDeque<Message>) {
		responses.add(GraphOperationMessage::Vector {
			layer,
			modification_type: VectorModificationType::ApplyPrimaryDelta { segment, delta },
		});
	}

	pub fn move_end(&self, segment: SegmentId, delta: DVec2, layer: LayerNodeIdentifier, responses: &mut VecDeque<Message>) {
		responses.add(GraphOperationMessage::Vector {
			layer,
			modification_type: VectorModificationType::ApplyEndDelta { segment, delta },
		});
	}

	pub fn move_anchor(&self, point: PointId, vector_data: &VectorData, delta: DVec2, layer: LayerNodeIdentifier, selected: Option<&SelectedLayerState>, responses: &mut VecDeque<Message>) {
		// Move anchor
		responses.add(GraphOperationMessage::Vector {
			layer,
			modification_type: VectorModificationType::ApplyPointDelta { point, delta },
		});

		// Move the other handle for a quadratic bezier
		for segment in vector_data.end_connected(point) {
			let Some((start, _end, bezier)) = vector_data.segment_points_from_id(segment) else { continue };

			if let BezierHandles::Quadratic { handle } = bezier.handles {
				if selected.is_some_and(|selected| selected.is_point_selected(ManipulatorPointId::Anchor(start))) {
					continue;
				}

				let relative_position = handle - bezier.start + delta;
				let modification_type = VectorModificationType::SetPrimaryHandle { segment, relative_position };

				responses.add(GraphOperationMessage::Vector { layer, modification_type });
			}
		}
	}

	/// Moves a control point to a `new_position` in document space.
	/// Returns `Some(())` if successful and `None` otherwise.
	pub fn reposition_control_point(
		&self,
		point: &ManipulatorPointId,
		network_interface: &NodeNetworkInterface,
		new_position: DVec2,
		layer: LayerNodeIdentifier,
		responses: &mut VecDeque<Message>,
	) -> Option<()> {
		if self.is_point_ignored(point) {
			return None;
		}

		let vector_data = network_interface.compute_modified_vector(layer)?;
		let transform = network_interface.document_metadata().transform_to_document_if_feeds(layer, network_interface).inverse();
		let position = transform.transform_point2(new_position);
		let current_position = point.get_position(&vector_data)?;
		let delta = position - current_position;

		match *point {
			ManipulatorPointId::Anchor(point) => self.move_anchor(point, &vector_data, delta, layer, None, responses),
			ManipulatorPointId::PrimaryHandle(segment) => {
				self.move_primary(segment, delta, layer, responses);
				if let Some(handle) = point.as_handle() {
					if let Some(handles) = vector_data.colinear_manipulators.iter().find(|handles| handles[0] == handle || handles[1] == handle) {
						let modification_type = VectorModificationType::SetG1Continuous { handles: *handles, enabled: false };
						responses.add(GraphOperationMessage::Vector { layer, modification_type });
					}
				}
			}
			ManipulatorPointId::EndHandle(segment) => {
				self.move_end(segment, delta, layer, responses);
				if let Some(handle) = point.as_handle() {
					if let Some(handles) = vector_data.colinear_manipulators.iter().find(|handles| handles[0] == handle || handles[1] == handle) {
						let modification_type = VectorModificationType::SetG1Continuous { handles: *handles, enabled: false };
						responses.add(GraphOperationMessage::Vector { layer, modification_type });
					}
				}
			}
		}

		Some(())
	}

	/// Iterates over the selected manipulator groups excluding endpoints, returning whether their handles have mixed, colinear, or free angles.
	/// If there are no points selected this function returns mixed.
	pub fn selected_manipulator_angles(&self, network_interface: &NodeNetworkInterface) -> ManipulatorAngle {
		// This iterator contains a bool indicating whether or not selected points' manipulator groups have colinear handles.
		let mut points_colinear_status = self
			.selected_shape_state
			.iter()
			.map(|(&layer, selection_state)| (network_interface.compute_modified_vector(layer), selection_state))
			.flat_map(|(data, selection_state)| {
				selection_state.selected_points.iter().filter_map(move |&point| {
					let Some(data) = &data else { return None };
					let _ = point.get_handle_pair(data)?; // ignores the endpoints.
					Some(data.colinear(point))
				})
			});

		let Some(first_is_colinear) = points_colinear_status.next() else { return ManipulatorAngle::Mixed };
		if points_colinear_status.any(|point| first_is_colinear != point) {
			return ManipulatorAngle::Mixed;
		}
		if first_is_colinear { ManipulatorAngle::Colinear } else { ManipulatorAngle::Free }
	}

	pub fn convert_manipulator_handles_to_colinear(&self, vector_data: &VectorData, point_id: PointId, responses: &mut VecDeque<Message>, layer: LayerNodeIdentifier) {
		let Some(anchor_position) = ManipulatorPointId::Anchor(point_id).get_position(vector_data) else {
			return;
		};
		let handles = vector_data.all_connected(point_id).take(2).collect::<Vec<_>>();
		let non_zero_handles = handles.iter().filter(|handle| handle.length(vector_data) > 1e-6).count();
		let handle_segments = handles.iter().map(|handles| handles.segment).collect::<Vec<_>>();

		// Check if the anchor is connected to linear segments and has no handles
		let linear_segments = vector_data.connected_linear_segments(point_id) != 0;

		// Grab the next and previous manipulator groups by simply looking at the next / previous index
		let points = handles.iter().map(|handle| vector_data.other_point(handle.segment, point_id));
		let anchor_positions = points
			.map(|point| point.and_then(|point| ManipulatorPointId::Anchor(point).get_position(vector_data)))
			.collect::<Vec<_>>();

		let mut segment_angle = 0.;
		let mut segment_count = 0.;

		for segment in &handle_segments {
			let Some(angle) = calculate_segment_angle(point_id, *segment, vector_data, false) else {
				continue;
			};
			segment_angle += angle;
			segment_count += 1.;
		}

		// For a non-endpoint anchor, handles are perpendicular to the average tangent of adjacent segments.(Refer:https://github.com/GraphiteEditor/Graphite/pull/2620#issuecomment-2881501494)
		let mut handle_direction = if segment_count > 1. {
			segment_angle /= segment_count;
			segment_angle += std::f64::consts::FRAC_PI_2;
			DVec2::new(segment_angle.cos(), segment_angle.sin())
		} else {
			DVec2::new(segment_angle.cos(), segment_angle.sin())
		};

		// Set the manipulator to have colinear handles
		if let (Some(a), Some(b)) = (handles.first(), handles.get(1)) {
			let handles = [*a, *b];
			let modification_type = VectorModificationType::SetG1Continuous { handles, enabled: true };
			responses.add(GraphOperationMessage::Vector { layer, modification_type });
		}

		// Flip the vector if it is not facing towards the same direction as the anchor
		let [first, second] = [anchor_positions.first().copied().flatten(), anchor_positions.get(1).copied().flatten()];
		if first.is_some_and(|group| (group - anchor_position).normalize_or_zero().dot(handle_direction) < 0.)
			|| second.is_some_and(|group| (group - anchor_position).normalize_or_zero().dot(handle_direction) > 0.)
		{
			handle_direction *= -1.;
		}

		if non_zero_handles != 0 && !linear_segments {
			let [a, b] = handles.as_slice() else { return };
			let (non_zero_handle, zero_handle) = if a.length(vector_data) > 1e-6 { (a, b) } else { (b, a) };
			let Some(direction) = non_zero_handle
				.to_manipulator_point()
				.get_position(vector_data)
				.and_then(|position| (position - anchor_position).try_normalize())
			else {
				return;
			};
			let new_position = -direction * non_zero_handle.length(vector_data);
			let modification_type = zero_handle.set_relative_position(new_position);
			responses.add(GraphOperationMessage::Vector { layer, modification_type });
		} else {
			// Push both in and out handles into the correct position
			for ((handle, sign), other_anchor) in handles.iter().zip([1., -1.]).zip(&anchor_positions) {
				let Some(anchor_vector) = other_anchor.map(|position| position - anchor_position) else {
					continue;
				};

				let Some(unit_vector) = anchor_vector.try_normalize() else {
					continue;
				};

				let projection = anchor_vector.length() * HANDLE_LENGTH_FACTOR * handle_direction.dot(unit_vector).abs();

				let new_position = handle_direction * projection * sign;
				let modification_type = handle.set_relative_position(new_position);
				responses.add(GraphOperationMessage::Vector { layer, modification_type });

				// Create the opposite handle if it doesn't exist (if it is not a cubic segment)
				if handle.opposite().to_manipulator_point().get_position(vector_data).is_none() {
					let modification_type = handle.opposite().set_relative_position(DVec2::ZERO);
					responses.add(GraphOperationMessage::Vector { layer, modification_type });
				}
			}
		}
	}

	/// Converts all selected points to colinear while moving the handles to ensure their 180° angle separation.
	/// If only one handle is selected, the other handle will be moved to match the angle of the selected handle.
	/// If both or neither handles are selected, the angle of both handles will be averaged from their current angles, weighted by their lengths.
	/// Assumes all selected manipulators have handles that are already not colinear.
	///
	/// For vector meshes, the non-colinear handle which is nearest in the direction of 180° angle separation becomes colinear with current handle.
	/// If there is no such handle, nothing happens.
	pub fn convert_selected_manipulators_to_colinear_handles(&self, responses: &mut VecDeque<Message>, document: &DocumentMessageHandler) {
		let mut skip_set = HashSet::new();

		for (&layer, layer_state) in self.selected_shape_state.iter() {
			let Some(vector_data) = document.network_interface.compute_modified_vector(layer) else {
				continue;
			};
			let transform = document.metadata().transform_to_document_if_feeds(layer, &document.network_interface);

			for &point in layer_state.selected_points.iter() {
				// Skip a point which has more than 2 segments connected (vector meshes)
				if let ManipulatorPointId::Anchor(anchor) = point {
					if vector_data.all_connected(anchor).count() > 2 {
						continue;
					}
				}

				// Here we take handles as the current handle and the most opposite non-colinear-handle

				let is_handle_colinear = |handle: HandleId| -> bool { vector_data.colinear_manipulators.iter().any(|&handles| handles[0] == handle || handles[1] == handle) };

				let other_handles = if matches!(point, ManipulatorPointId::Anchor(_)) {
					point.get_handle_pair(&vector_data)
				} else {
					point.get_all_connected_handles(&vector_data).and_then(|handles| {
						let mut non_colinear_handles = handles.iter().filter(|&handle| !is_handle_colinear(*handle)).clone().collect::<Vec<_>>();

						// Sort these by angle from the current handle
						non_colinear_handles.sort_by(|&handle_a, &handle_b| {
							let anchor = point.get_anchor_position(&vector_data).expect("No anchor position for handle");
							let orig_handle_pos = point.get_position(&vector_data).expect("No handle position");

							let a_pos = handle_a.to_manipulator_point().get_position(&vector_data).expect("No handle position");
							let b_pos = handle_b.to_manipulator_point().get_position(&vector_data).expect("No handle position");

							let v_orig = (orig_handle_pos - anchor).normalize_or_zero();

							let v_a = (a_pos - anchor).normalize_or_zero();
							let v_b = (b_pos - anchor).normalize_or_zero();

							let angle_a = v_orig.angle_to(v_a).abs();
							let angle_b = v_orig.angle_to(v_b).abs();

							// Sort by descending angle (180° is furthest)
							angle_b.partial_cmp(&angle_a).unwrap_or(std::cmp::Ordering::Equal)
						});

						let current = match point {
							ManipulatorPointId::EndHandle(segment) => HandleId::end(segment),
							ManipulatorPointId::PrimaryHandle(segment) => HandleId::primary(segment),
							ManipulatorPointId::Anchor(_) => unreachable!(),
						};

						non_colinear_handles.first().map(|other| [current, **other])
					})
				};

				let Some(handles) = other_handles else { continue };

				if skip_set.contains(&handles) || skip_set.contains(&[handles[1], handles[0]]) {
					continue;
				};

				skip_set.insert(handles);

				let [selected0, selected1] = handles.map(|handle| layer_state.selected_points.contains(&handle.to_manipulator_point()));
				let handle_positions = handles.map(|handle| handle.to_manipulator_point().get_position(&vector_data));

				let Some(anchor_id) = point.get_anchor(&vector_data) else { continue };
				let Some(anchor) = vector_data.point_domain.position_from_id(anchor_id) else { continue };

				let anchor_points = handles.map(|handle| vector_data.other_point(handle.segment, anchor_id));
				let anchor_positions = anchor_points.map(|point| point.and_then(|point| vector_data.point_domain.position_from_id(point)));

				// If one handle is selected (but both exist), only move the other handle
				if let (true, [Some(pos0), Some(pos1)]) = ((selected0 ^ selected1), handle_positions) {
					let [(_selected_handle, selected_position), (unselected_handle, unselected_position)] = if selected0 {
						[(handles[0], pos0), (handles[1], pos1)]
					} else {
						[(handles[1], pos1), (handles[0], pos0)]
					};
					let direction = transform
						.transform_vector2(anchor - selected_position)
						.try_normalize()
						.unwrap_or_else(|| transform.transform_vector2(unselected_position - anchor).normalize_or_zero());

					let length = transform.transform_vector2(unselected_position - anchor).length();
					let position = transform.inverse().transform_vector2(direction * length);
					let modification_type = unselected_handle.set_relative_position(position);
					if (anchor - selected_position).length() > 1e-6 {
						responses.add(GraphOperationMessage::Vector { layer, modification_type });
					}
				}
				// If both handles are selected, average the angles of the handles
				else {
					// We could normalize these directions?
					let mut handle_directions = handle_positions.map(|handle| handle.map(|handle| handle - anchor));

					let mut normalized = handle_directions[0].and_then(|a| handle_directions[1].and_then(|b| (a - b).try_normalize()));

					if normalized.is_none() || handle_directions.iter().any(|&d| d.is_some_and(|d| d.length_squared() < f64::EPSILON * 1e5)) {
						handle_directions = anchor_positions.map(|relative_anchor| relative_anchor.map(|relative_anchor| (relative_anchor - anchor) / 3.));
						normalized = handle_directions[0].and_then(|a| handle_directions[1].and_then(|b| (a - b).try_normalize()))
					}
					let Some(normalized) = normalized else { continue };

					// Push both in and out handles into the correct position
					for (index, sign) in [(0, 1.), (1, -1.)] {
						let Some(direction) = handle_directions[index] else { continue };
						let new_position = direction.length() * normalized * sign;
						let modification_type = handles[index].set_relative_position(new_position);
						responses.add(GraphOperationMessage::Vector { layer, modification_type });

						// Create the opposite handle if it doesn't exist (if it is not a cubic segment)
						if handles[index].opposite().to_manipulator_point().get_position(&vector_data).is_none() {
							let modification_type = handles[index].opposite().set_relative_position(DVec2::ZERO);
							responses.add(GraphOperationMessage::Vector { layer, modification_type });
						}
					}
				}
				let modification_type = VectorModificationType::SetG1Continuous { handles, enabled: true };
				responses.add(GraphOperationMessage::Vector { layer, modification_type });
			}
		}
	}

	/// Move the selected points and segments by dragging the mouse.
	#[allow(clippy::too_many_arguments)]
	pub fn move_selected_points_and_segments(
		&self,
		handle_lengths: Option<OpposingHandleLengths>,
		document: &DocumentMessageHandler,
		delta: DVec2,
		equidistant: bool,
		in_viewport_space: bool,
		was_alt_dragging: bool,
		opposite_handle_position: Option<DVec2>,
		skip_opposite_handle: bool,
		responses: &mut VecDeque<Message>,
	) {
		for (&layer, state) in &self.selected_shape_state {
			let Some(vector_data) = document.network_interface.compute_modified_vector(layer) else { continue };

			let opposing_handles = handle_lengths.as_ref().and_then(|handle_lengths| handle_lengths.get(&layer));

			let transform_to_viewport_space = document.metadata().transform_to_viewport_if_feeds(layer, &document.network_interface);
			let transform_to_document_space = document.metadata().transform_to_document_if_feeds(layer, &document.network_interface);
			let delta_transform = if in_viewport_space {
				transform_to_viewport_space
			} else {
				DAffine2::from_angle(document.document_ptz.tilt()) * transform_to_document_space
			};
			let delta = delta_transform.inverse().transform_vector2(delta);

			// Make a new collection of anchor points which needs to be moved
			let mut affected_points = state.selected_points.clone();

			for (segment_id, _, start, end) in vector_data.segment_bezier_iter() {
				if state.is_segment_selected(segment_id) {
					affected_points.insert(ManipulatorPointId::Anchor(start));
					affected_points.insert(ManipulatorPointId::Anchor(end));
				}
			}

			for &point in affected_points.iter() {
				if self.is_point_ignored(&point) {
					continue;
				}

				let handle = match point {
					ManipulatorPointId::Anchor(point) => {
						self.move_anchor(point, &vector_data, delta, layer, Some(state), responses);
						continue;
					}
					ManipulatorPointId::PrimaryHandle(segment) => HandleId::primary(segment),
					ManipulatorPointId::EndHandle(segment) => HandleId::end(segment),
				};

				let Some(anchor_id) = point.get_anchor(&vector_data) else { continue };
				if state.is_point_selected(ManipulatorPointId::Anchor(anchor_id)) {
					continue;
				}

				let Some(anchor_position) = vector_data.point_domain.position_from_id(anchor_id) else { continue };

				let Some(handle_position) = point.get_position(&vector_data) else { continue };
				let handle_position = handle_position + delta;

				let modification_type = handle.set_relative_position(handle_position - anchor_position);

				responses.add(GraphOperationMessage::Vector { layer, modification_type });

				let Some(other) = vector_data.other_colinear_handle(handle) else { continue };

				if skip_opposite_handle {
					continue;
				}

				if state.is_point_selected(other.to_manipulator_point()) {
					// If two colinear handles are being dragged at the same time but not the anchor, it is necessary to break the colinear state.
					let handles = [handle, other];
					let modification_type = VectorModificationType::SetG1Continuous { handles, enabled: false };
					responses.add(GraphOperationMessage::Vector { layer, modification_type });
					continue;
				}

				let new_relative = if equidistant {
					-(handle_position - anchor_position)
				}
				// If the handle is very close to the anchor, return the original position
				else if (handle_position - anchor_position).length_squared() < f64::EPSILON * 1e5 {
					let Some(opposite_handle_position) = opposite_handle_position else { continue };
					opposite_handle_position - anchor_position
				} else {
					// TODO: Is this equivalent to `transform_to_document_space`? If changed, the before and after should be tested.
					let transform = document.metadata().document_to_viewport.inverse() * transform_to_viewport_space;
					let Some(other_position) = other.to_manipulator_point().get_position(&vector_data) else {
						continue;
					};
					let direction = transform.transform_vector2(handle_position - anchor_position).try_normalize();
					let opposing_handle = opposing_handles.and_then(|handles| handles.get(&other));
					let length = opposing_handle.copied().unwrap_or_else(|| transform.transform_vector2(other_position - anchor_position).length());
					direction.map_or(other_position - anchor_position, |direction| transform.inverse().transform_vector2(-direction * length))
				};

				if !was_alt_dragging {
					let modification_type = other.set_relative_position(new_relative);
					responses.add(GraphOperationMessage::Vector { layer, modification_type });
				}
			}
		}
	}

	/// The opposing handle lengths.
	pub fn opposing_handle_lengths(&self, document: &DocumentMessageHandler) -> OpposingHandleLengths {
		self.selected_shape_state
			.iter()
			.filter_map(|(&layer, state)| {
				let vector_data = document.network_interface.compute_modified_vector(layer)?;
				let transform = document.metadata().transform_to_document_if_feeds(layer, &document.network_interface);
				let opposing_handle_lengths = vector_data
					.colinear_manipulators
					.iter()
					.filter_map(|&handles| {
						// We will keep track of the opposing handle length when:
						// i) Exactly one handle is selected.
						// ii) The anchor is not selected.

						let anchor = handles[0].to_manipulator_point().get_anchor(&vector_data)?;
						let anchor_selected = state.is_point_selected(ManipulatorPointId::Anchor(anchor));
						if anchor_selected {
							return None;
						}

						let handles_selected = handles.map(|handle| state.is_point_selected(handle.to_manipulator_point()));

						let other = match handles_selected {
							[true, false] => handles[1],
							[false, true] => handles[0],
							_ => return None,
						};

						let opposing_handle_position = other.to_manipulator_point().get_position(&vector_data)?;
						let anchor_position = vector_data.point_domain.position_from_id(anchor)?;

						let opposing_handle_length = transform.transform_vector2(opposing_handle_position - anchor_position).length();
						Some((other, opposing_handle_length))
					})
					.collect::<HashMap<_, _>>();
				Some((layer, opposing_handle_lengths))
			})
			.collect::<HashMap<_, _>>()
	}

	pub fn dissolve_segment(&self, responses: &mut VecDeque<Message>, layer: LayerNodeIdentifier, vector_data: &VectorData, segment: SegmentId, points: [PointId; 2]) {
		// Checking which point is terminal point
		let is_point1_terminal = vector_data.connected_count(points[0]) == 1;
		let is_point2_terminal = vector_data.connected_count(points[1]) == 1;

		// Delete the segment and terminal points
		let modification_type = VectorModificationType::RemoveSegment { id: segment };
		responses.add(GraphOperationMessage::Vector { layer, modification_type });
		for &handles in vector_data.colinear_manipulators.iter().filter(|handles| handles.iter().any(|handle| handle.segment == segment)) {
			let modification_type = VectorModificationType::SetG1Continuous { handles, enabled: false };
			responses.add(GraphOperationMessage::Vector { layer, modification_type });
		}

		if is_point1_terminal {
			let modification_type = VectorModificationType::RemovePoint { id: points[0] };
			responses.add(GraphOperationMessage::Vector { layer, modification_type });
		}
		if is_point2_terminal {
			let modification_type = VectorModificationType::RemovePoint { id: points[1] };
			responses.add(GraphOperationMessage::Vector { layer, modification_type });
		}
	}

	fn dissolve_anchor(anchor: PointId, responses: &mut VecDeque<Message>, layer: LayerNodeIdentifier, vector_data: &VectorData) -> Option<[(HandleId, PointId); 2]> {
		// Delete point
		let modification_type = VectorModificationType::RemovePoint { id: anchor };
		responses.add(GraphOperationMessage::Vector { layer, modification_type });

		// Delete connected segments
		for HandleId { segment, .. } in vector_data.all_connected(anchor) {
			let modification_type = VectorModificationType::RemoveSegment { id: segment };
			responses.add(GraphOperationMessage::Vector { layer, modification_type });
			for &handles in vector_data.colinear_manipulators.iter().filter(|handles| handles.iter().any(|handle| handle.segment == segment)) {
				let modification_type = VectorModificationType::SetG1Continuous { handles, enabled: false };
				responses.add(GraphOperationMessage::Vector { layer, modification_type });
			}
		}

		// Add in new segment if possible
		let mut handles = ManipulatorPointId::Anchor(anchor).get_handle_pair(vector_data)?;
		handles.reverse();
		let opposites = handles.map(|handle| handle.opposite());

		let [Some(start), Some(end)] = opposites.map(|opposite| opposite.to_manipulator_point().get_anchor(vector_data)) else {
			return None;
		};
		Some([(handles[0], start), (handles[1], end)])
	}

	/// Dissolve the selected points.
	pub fn delete_selected_points(&mut self, document: &DocumentMessageHandler, responses: &mut VecDeque<Message>) {
		for (&layer, state) in &mut self.selected_shape_state {
			let mut missing_anchors = HashMap::new();
			let mut deleted_anchors = HashSet::new();
			let Some(vector_data) = document.network_interface.compute_modified_vector(layer) else {
				continue;
			};

			let selected_segments = &state.selected_segments;

			for point in std::mem::take(&mut state.selected_points) {
				match point {
					ManipulatorPointId::Anchor(anchor) => {
						if let Some(handles) = Self::dissolve_anchor(anchor, responses, layer, &vector_data) {
							if !vector_data.all_connected(anchor).any(|a| selected_segments.contains(&a.segment)) && vector_data.all_connected(anchor).count() <= 2 {
								missing_anchors.insert(anchor, handles);
							}
						}
						deleted_anchors.insert(anchor);
					}
					ManipulatorPointId::PrimaryHandle(_) | ManipulatorPointId::EndHandle(_) => {
						let Some(handle) = point.as_handle() else { continue };

						// Place the handle on top of the anchor
						let modification_type = handle.set_relative_position(DVec2::ZERO);
						responses.add(GraphOperationMessage::Vector { layer, modification_type });

						// Disable the g1 continuous
						for &handles in &vector_data.colinear_manipulators {
							if handles.contains(&handle) {
								let modification_type = VectorModificationType::SetG1Continuous { handles, enabled: false };
								responses.add(GraphOperationMessage::Vector { layer, modification_type });
							}
						}
					}
				}
			}

			let mut visited = Vec::new();
			while let Some((anchor, handles)) = missing_anchors.keys().next().copied().and_then(|id| missing_anchors.remove_entry(&id)) {
				visited.push(anchor);

				// If the adjacent point is just this point then skip
				let mut handles = handles.map(|handle| (handle.1 != anchor).then_some(handle));

				// If the adjacent points are themselves being deleted, then repeatedly visit the newest agacent points.
				for handle in &mut handles {
					while let Some((point, connected)) = (*handle).and_then(|(_, point)| missing_anchors.remove_entry(&point)) {
						visited.push(point);

						*handle = connected.into_iter().find(|(_, point)| !visited.contains(point));
					}
				}

				let [Some(start), Some(end)] = handles else { continue };

				// Avoid reconnecting to points that are being deleted (this can happen if a whole loop is deleted)
				if deleted_anchors.contains(&start.1) || deleted_anchors.contains(&end.1) {
					continue;
				}

				// Avoid reconnecting to points which have adjacent segments selected

				// Grab the handles from the opposite side of the segment(s) being deleted and make it relative to the anchor
				let [handle_start, handle_end] = [start, end].map(|(handle, _)| {
					let handle = handle.opposite();
					let handle_position = handle.to_manipulator_point().get_position(&vector_data);
					let relative_position = handle
						.to_manipulator_point()
						.get_anchor(&vector_data)
						.and_then(|anchor| vector_data.point_domain.position_from_id(anchor));
					handle_position.and_then(|handle| relative_position.map(|relative| handle - relative)).unwrap_or_default()
				});

				let segment = start.0.segment;

				let modification_type = VectorModificationType::InsertSegment {
					id: segment,
					points: [start.1, end.1],
					handles: [Some(handle_start), Some(handle_end)],
				};

				responses.add(GraphOperationMessage::Vector { layer, modification_type });

				for &handles in vector_data.colinear_manipulators.iter() {
					if !handles.iter().any(|&handle| handle == start.0.opposite() || handle == end.0.opposite()) {
						continue;
					}

					let Some(anchor) = handles[0].to_manipulator_point().get_anchor(&vector_data) else { continue };
					let Some(other) = handles.iter().find(|&&handle| handle != start.0.opposite() && handle != end.0.opposite()) else {
						continue;
					};

					let handle_ty = if anchor == start.1 {
						HandleId::primary(segment)
					} else if anchor == end.1 {
						HandleId::end(segment)
					} else {
						continue;
					};
					let handles = [*other, handle_ty];
					let modification_type = VectorModificationType::SetG1Continuous { handles, enabled: true };

					responses.add(GraphOperationMessage::Vector { layer, modification_type });
				}
			}
		}
	}

	pub fn delete_selected_segments(&mut self, document: &DocumentMessageHandler, responses: &mut VecDeque<Message>) {
		for (&layer, state) in &self.selected_shape_state {
			let Some(vector_data) = document.network_interface.compute_modified_vector(layer) else {
				continue;
			};

			for (segment, _, start, end) in vector_data.segment_bezier_iter() {
				if state.selected_segments.contains(&segment) {
					self.dissolve_segment(responses, layer, &vector_data, segment, [start, end]);
				}
			}
		}
	}

	pub fn break_path_at_selected_point(&self, document: &DocumentMessageHandler, responses: &mut VecDeque<Message>) {
		for (&layer, state) in &self.selected_shape_state {
			let Some(vector_data) = document.network_interface.compute_modified_vector(layer) else { continue };

			for &delete in &state.selected_points {
				let Some(point) = delete.get_anchor(&vector_data) else { continue };
				let Some(pos) = vector_data.point_domain.position_from_id(point) else { continue };

				let mut used_initial_point = false;
				for handle in vector_data.all_connected(point) {
					// Disable the g1 continuous
					for &handles in &vector_data.colinear_manipulators {
						if handles.contains(&handle) {
							let modification_type = VectorModificationType::SetG1Continuous { handles, enabled: false };
							responses.add(GraphOperationMessage::Vector { layer, modification_type });
						}
					}

					// Keep the existing point for the first segment
					if !used_initial_point {
						used_initial_point = true;
						continue;
					}

					// Create new point
					let id = PointId::generate();
					let modification_type = VectorModificationType::InsertPoint { id, position: pos };

					responses.add(GraphOperationMessage::Vector { layer, modification_type });

					// Update segment
					let HandleId { ty, segment } = handle;
					let modification_type = match ty {
						graphene_std::vector::HandleType::Primary => VectorModificationType::SetStartPoint { segment, id },
						graphene_std::vector::HandleType::End => VectorModificationType::SetEndPoint { segment, id },
					};

					responses.add(GraphOperationMessage::Vector { layer, modification_type });
				}
			}
		}
	}

	/// Delete point(s) and adjacent segments.
	pub fn delete_point_and_break_path(&mut self, document: &DocumentMessageHandler, responses: &mut VecDeque<Message>) {
		for (&layer, state) in &mut self.selected_shape_state {
			let Some(vector_data) = document.network_interface.compute_modified_vector(layer) else {
				continue;
			};

			for delete in std::mem::take(&mut state.selected_points) {
				let Some(point) = delete.get_anchor(&vector_data) else { continue };

				// Delete point
				let modification_type = VectorModificationType::RemovePoint { id: point };
				responses.add(GraphOperationMessage::Vector { layer, modification_type });

				// Delete connected segments
				for HandleId { segment, .. } in vector_data.all_connected(point) {
					let modification_type = VectorModificationType::RemoveSegment { id: segment };
					responses.add(GraphOperationMessage::Vector { layer, modification_type });
				}
			}
		}
	}

	/// Disable colinear handles colinear.
	pub fn disable_colinear_handles_state_on_selected(&self, network_interface: &NodeNetworkInterface, responses: &mut VecDeque<Message>) {
		for (&layer, state) in &self.selected_shape_state {
			let Some(vector_data) = network_interface.compute_modified_vector(layer) else { continue };

			for &point in &state.selected_points {
				if let ManipulatorPointId::Anchor(point) = point {
					for connected in vector_data.all_connected(point) {
						if let Some(&handles) = vector_data.colinear_manipulators.iter().find(|target| target.contains(&connected)) {
							let modification_type = VectorModificationType::SetG1Continuous { handles, enabled: false };
							responses.add(GraphOperationMessage::Vector { layer, modification_type });
						}
					}
				} else if let Some(handle) = point.as_handle() {
					if let Some(handles) = vector_data.colinear_manipulators.iter().find(|handles| handles[0] == handle || handles[1] == handle) {
						let modification_type = VectorModificationType::SetG1Continuous { handles: *handles, enabled: false };
						responses.add(GraphOperationMessage::Vector { layer, modification_type });
					}
				}
			}
		}
	}

	/// Find a [ManipulatorPoint] that is within the selection threshold and return the layer path, an index to the [ManipulatorGroup], and an enum index for [ManipulatorPoint].
	pub fn find_nearest_point_indices(&mut self, network_interface: &NodeNetworkInterface, mouse_position: DVec2, select_threshold: f64) -> Option<(LayerNodeIdentifier, ManipulatorPointId)> {
		if self.selected_shape_state.is_empty() {
			return None;
		}

		let select_threshold_squared = select_threshold * select_threshold;

		// Find the closest control point among all elements of shapes_to_modify
		for &layer in self.selected_shape_state.keys() {
			if let Some((manipulator_point_id, distance_squared)) = Self::closest_point_in_layer(network_interface, layer, mouse_position) {
				// Choose the first point under the threshold
				if distance_squared < select_threshold_squared {
					trace!("Selecting... manipulator point: {manipulator_point_id:?}");
					return Some((layer, manipulator_point_id));
				}
			}
		}

		None
	}

	pub fn find_nearest_visible_point_indices(
		&mut self,
		network_interface: &NodeNetworkInterface,
		mouse_position: DVec2,
		select_threshold: f64,
		path_overlay_mode: PathOverlayMode,
		frontier_handles_info: Option<HashMap<SegmentId, Vec<PointId>>>,
	) -> Option<(LayerNodeIdentifier, ManipulatorPointId)> {
		if self.selected_shape_state.is_empty() {
			return None;
		}

		let select_threshold_squared = select_threshold.powi(2);

		// Find the closest control point among all elements of shapes_to_modify
		for &layer in self.selected_shape_state.keys() {
			if let Some((manipulator_point_id, distance_squared)) = Self::closest_point_in_layer(network_interface, layer, mouse_position) {
				// Choose the first point under the threshold
				if distance_squared < select_threshold_squared {
					// Check if point is visible in current PathOverlayMode
					let vector_data = network_interface.compute_modified_vector(layer)?;
					let selected_segments = selected_segments(network_interface, self);
					let selected_points = self.selected_points().cloned().collect::<HashSet<_>>();

					if !is_visible_point(manipulator_point_id, &vector_data, path_overlay_mode, frontier_handles_info, selected_segments, &selected_points) {
						return None;
					}

					return Some((layer, manipulator_point_id));
				}
			}
		}

		None
	}

	// TODO Use quadtree or some equivalent spatial acceleration structure to improve this to O(log(n))
	/// Find the closest manipulator, manipulator point, and distance so we can select path elements.
	/// Brute force comparison to determine which manipulator (handle or anchor) we want to select taking O(n) time.
	/// Return value is an `Option` of the tuple representing `(ManipulatorPointId, distance squared)`.
	fn closest_point_in_layer(network_interface: &NodeNetworkInterface, layer: LayerNodeIdentifier, pos: glam::DVec2) -> Option<(ManipulatorPointId, f64)> {
		let mut closest_distance_squared: f64 = f64::MAX;
		let mut manipulator_point = None;

		let vector_data = network_interface.compute_modified_vector(layer)?;
		let viewspace = network_interface.document_metadata().transform_to_viewport_if_feeds(layer, network_interface);

		// Handles
		for (segment_id, bezier, _, _) in vector_data.segment_bezier_iter() {
			let bezier = bezier.apply_transformation(|point| viewspace.transform_point2(point));
			let valid = |handle: DVec2, control: DVec2| handle.distance_squared(control) > crate::consts::HIDE_HANDLE_DISTANCE.powi(2);

			if let Some(primary_handle) = bezier.handle_start() {
				if valid(primary_handle, bezier.start) && (bezier.handle_end().is_some() || valid(primary_handle, bezier.end)) && primary_handle.distance_squared(pos) <= closest_distance_squared {
					closest_distance_squared = primary_handle.distance_squared(pos);
					manipulator_point = Some(ManipulatorPointId::PrimaryHandle(segment_id));
				}
			}
			if let Some(end_handle) = bezier.handle_end() {
				if valid(end_handle, bezier.end) && end_handle.distance_squared(pos) <= closest_distance_squared {
					closest_distance_squared = end_handle.distance_squared(pos);
					manipulator_point = Some(ManipulatorPointId::EndHandle(segment_id));
				}
			}
		}

		// Anchors
		for (&id, &point) in vector_data.point_domain.ids().iter().zip(vector_data.point_domain.positions()) {
			let point = viewspace.transform_point2(point);

			if point.distance_squared(pos) <= closest_distance_squared {
				closest_distance_squared = point.distance_squared(pos);
				manipulator_point = Some(ManipulatorPointId::Anchor(id));
			}
		}

		manipulator_point.map(|id| (id, closest_distance_squared))
	}

	/// Find the `t` value along the path segment we have clicked upon, together with that segment ID.
	fn closest_segment(&self, network_interface: &NodeNetworkInterface, layer: LayerNodeIdentifier, position: glam::DVec2, tolerance: f64) -> Option<ClosestSegment> {
		let transform = network_interface.document_metadata().transform_to_viewport_if_feeds(layer, network_interface);
		let layer_pos = transform.inverse().transform_point2(position);

		let tolerance = tolerance + 0.5;

		let mut closest = None;
		let mut closest_distance_squared: f64 = tolerance * tolerance;

		let vector_data = network_interface.compute_modified_vector(layer)?;

		for (segment, mut bezier, start, end) in vector_data.segment_bezier_iter() {
			let t = bezier.project(layer_pos);
			let layerspace = bezier.evaluate(TValue::Parametric(t));

			let screenspace = transform.transform_point2(layerspace);
			let distance_squared = screenspace.distance_squared(position);

			if distance_squared < closest_distance_squared {
				closest_distance_squared = distance_squared;

				// Convert to linear if handes are on top of control points
				if let bezier_rs::BezierHandles::Cubic { handle_start, handle_end } = bezier.handles {
					if handle_start.abs_diff_eq(bezier.start(), f64::EPSILON * 100.) && handle_end.abs_diff_eq(bezier.end(), f64::EPSILON * 100.) {
						bezier = Bezier::from_linear_dvec2(bezier.start, bezier.end);
					}
				}

				let primary_handle = vector_data.colinear_manipulators.iter().find(|handles| handles.contains(&HandleId::primary(segment)));
				let end_handle = vector_data.colinear_manipulators.iter().find(|handles| handles.contains(&HandleId::end(segment)));
				let primary_handle = primary_handle.and_then(|&handles| handles.into_iter().find(|handle| handle.segment != segment));
				let end_handle = end_handle.and_then(|&handles| handles.into_iter().find(|handle| handle.segment != segment));

				closest = Some(ClosestSegment {
					segment,
					bezier,
					points: [start, end],
					colinear: [primary_handle, end_handle],
					t,
					bezier_point_to_viewport: screenspace,
					layer,
				});
			}
		}

		closest
	}

	/// find closest to the position segment on selected layers. If there is more than one layers with close enough segment it return upper from them
	pub fn upper_closest_segment(&self, network_interface: &NodeNetworkInterface, position: glam::DVec2, tolerance: f64) -> Option<ClosestSegment> {
		let closest_seg = |layer| self.closest_segment(network_interface, layer, position, tolerance);
		match self.selected_shape_state.len() {
			0 => None,
			1 => self.selected_layers().next().copied().and_then(closest_seg),
			_ => self.sorted_selected_layers(network_interface.document_metadata()).find_map(closest_seg),
		}
	}
	pub fn get_dragging_state(&self, network_interface: &NodeNetworkInterface) -> PointSelectState {
		for &layer in self.selected_shape_state.keys() {
			let Some(vector_data) = network_interface.compute_modified_vector(layer) else { continue };

			for point in self.selected_points() {
				if point.as_anchor().is_some() {
					return PointSelectState::Anchor;
				}
				if point.get_handle_pair(&vector_data).is_some() {
					return PointSelectState::HandleWithPair;
				}
			}
		}
		PointSelectState::HandleNoPair
	}

	/// Returns true if at least one handle with pair is selected
	pub fn handle_with_pair_selected(&mut self, network_interface: &NodeNetworkInterface) -> bool {
		for &layer in self.selected_shape_state.keys() {
			let Some(vector_data) = network_interface.compute_modified_vector(layer) else { continue };

			for point in self.selected_points() {
				if point.as_anchor().is_some() {
					return false;
				}
				if point.get_handle_pair(&vector_data).is_some() {
					return true;
				}
			}
		}

		false
	}

	/// Alternate selected handles to mirrors
	pub fn alternate_selected_handles(&mut self, network_interface: &NodeNetworkInterface) {
		let mut handles_to_update = Vec::new();

		for &layer in self.selected_shape_state.keys() {
			let Some(vector_data) = network_interface.compute_modified_vector(layer) else { continue };

			for point in self.selected_points() {
				if point.as_anchor().is_some() {
					continue;
				}

				if let Some(other_handles) = point.get_all_connected_handles(&vector_data) {
					// Find the next closest handle in the clockwise sense
					let mut candidates = other_handles.clone();
					candidates.sort_by(|&handle_a, &handle_b| {
						let anchor = point.get_anchor_position(&vector_data).expect("No anchor position for handle");
						let orig_handle_pos = point.get_position(&vector_data).expect("No handle position");

						let a_pos = handle_a.to_manipulator_point().get_position(&vector_data).expect("No handle position");
						let b_pos = handle_b.to_manipulator_point().get_position(&vector_data).expect("No handle position");

						let v_orig = (orig_handle_pos - anchor).normalize_or_zero();

						let v_a = (a_pos - anchor).normalize_or_zero();
						let v_b = (b_pos - anchor).normalize_or_zero();

						let signed_angle = |base: DVec2, to: DVec2| -> f64 {
							let angle = base.angle_to(to);
							let cross = base.perp_dot(to);

							if cross < 0. { TAU - angle } else { angle }
						};

						let angle_a = signed_angle(v_orig, v_a);
						let angle_b = signed_angle(v_orig, v_b);

						angle_a.partial_cmp(&angle_b).unwrap_or(std::cmp::Ordering::Equal)
					});

					if candidates.is_empty() {
						continue;
					}

					handles_to_update.push((layer, *point, candidates[0].to_manipulator_point()));
				}
			}
		}

		for (layer, handle_to_deselect, handle_to_select) in handles_to_update {
			if let Some(state) = self.selected_shape_state.get_mut(&layer) {
				let points = &state.selected_points;
				let both_selected = points.contains(&handle_to_deselect) && points.contains(&handle_to_select);
				if both_selected {
					continue;
				}

				state.deselect_point(handle_to_deselect);
				state.select_point(handle_to_select);
			}
		}
	}

	/// Selects handles and anchor connected to current handle
	pub fn select_handles_and_anchor_connected_to_current_handle(&mut self, network_interface: &NodeNetworkInterface) {
		let mut points_to_select: Vec<(LayerNodeIdentifier, Option<PointId>, Option<ManipulatorPointId>)> = Vec::new();

		for &layer in self.selected_shape_state.keys() {
			let Some(vector_data) = network_interface.compute_modified_vector(layer) else {
				continue;
			};

			for point in self.selected_points().filter(|point| point.as_handle().is_some()) {
				let anchor = point.get_anchor(&vector_data);
				match point.get_handle_pair(&vector_data) {
					Some(handles) => {
						points_to_select.push((layer, anchor, Some(handles[1].to_manipulator_point())));
					}
					_ => {
						points_to_select.push((layer, anchor, None));
					}
				}
			}
		}

		for (layer, anchor, handle) in points_to_select {
			if let Some(state) = self.selected_shape_state.get_mut(&layer) {
				if let Some(anchor) = anchor {
					state.select_point(ManipulatorPointId::Anchor(anchor));
				}
				if let Some(handle) = handle {
					state.select_point(handle);
				}
			}
		}
	}

	pub fn select_points_by_manipulator_id(&mut self, points: &Vec<ManipulatorPointId>) {
		let layers_to_modify: Vec<_> = self.selected_shape_state.keys().cloned().collect();

		for layer in layers_to_modify {
			if let Some(state) = self.selected_shape_state.get_mut(&layer) {
				for point in points {
					state.select_point(*point);
				}
			}
		}
	}

	/// Converts a nearby clicked anchor point's handles between sharp (zero-length handles) and smooth (pulled-apart handle(s)).
	/// If both handles aren't zero-length, they are set that. If both are zero-length, they are stretched apart by a reasonable amount.
	/// This can can be activated by double clicking on an anchor with the Path tool.
	pub fn flip_smooth_sharp(&self, network_interface: &NodeNetworkInterface, target: glam::DVec2, tolerance: f64, responses: &mut VecDeque<Message>) -> bool {
		let mut process_layer = |layer| {
			let vector_data = network_interface.compute_modified_vector(layer)?;
			let transform_to_screenspace = network_interface.document_metadata().transform_to_viewport_if_feeds(layer, network_interface);

			let mut result = None;
			let mut closest_distance_squared = tolerance * tolerance;

			// Find the closest anchor point on the current layer
			for (&id, &anchor) in vector_data.point_domain.ids().iter().zip(vector_data.point_domain.positions()) {
				let screenspace = transform_to_screenspace.transform_point2(anchor);
				let distance_squared = screenspace.distance_squared(target);

				if distance_squared < closest_distance_squared {
					closest_distance_squared = distance_squared;
					result = Some((id, anchor));
				}
			}

			let (id, anchor) = result?;
			let handles = vector_data.all_connected(id);
			let positions = handles
				.filter_map(|handle| handle.to_manipulator_point().get_position(&vector_data))
				.filter(|&handle| anchor.abs_diff_eq(handle, 1e-5))
				.count();

			// Check if the anchor is connected to linear segments.
			let one_or_more_segment_linear = vector_data.connected_linear_segments(id) != 0;

			// Check by comparing the handle positions to the anchor if this manipulator group is a point
			for point in self.selected_points() {
				let Some(point_id) = point.as_anchor() else { continue };
				if positions != 0 || one_or_more_segment_linear {
					self.convert_manipulator_handles_to_colinear(&vector_data, point_id, responses, layer);
				} else {
					for handle in vector_data.all_connected(point_id) {
						let Some(bezier) = vector_data.segment_from_id(handle.segment) else { continue };

						match bezier.handles {
							BezierHandles::Linear => {}
							BezierHandles::Quadratic { .. } => {
								let segment = handle.segment;
								// Convert to linear
								let modification_type = VectorModificationType::SetHandles { segment, handles: [None; 2] };
								responses.add(GraphOperationMessage::Vector { layer, modification_type });

								// Set the manipulator to have non-colinear handles
								for &handles in &vector_data.colinear_manipulators {
									if handles.contains(&HandleId::primary(segment)) {
										let modification_type = VectorModificationType::SetG1Continuous { handles, enabled: false };
										responses.add(GraphOperationMessage::Vector { layer, modification_type });
									}
								}
							}
							BezierHandles::Cubic { .. } => {
								// Set handle position to anchor position
								let modification_type = handle.set_relative_position(DVec2::ZERO);
								responses.add(GraphOperationMessage::Vector { layer, modification_type });

								// Set the manipulator to have non-colinear handles
								for &handles in &vector_data.colinear_manipulators {
									if handles.contains(&handle) {
										let modification_type = VectorModificationType::SetG1Continuous { handles, enabled: false };
										responses.add(GraphOperationMessage::Vector { layer, modification_type });
									}
								}
							}
						}
					}
				};
			}

			Some(true)
		};

		for &layer in self.selected_shape_state.keys() {
			if let Some(result) = process_layer(layer) {
				return result;
			}
		}

		false
	}

	#[allow(clippy::too_many_arguments)]
	pub fn select_all_in_shape(
		&mut self,
		network_interface: &NodeNetworkInterface,
		selection_shape: SelectionShape,
		selection_change: SelectionChange,
		path_overlay_mode: PathOverlayMode,
		frontier_handles_info: Option<HashMap<SegmentId, Vec<PointId>>>,
		select_segments: bool,
		// Here, "selection mode" represents touched or enclosed, not to be confused with editing modes
		selection_mode: SelectionMode,
	) {
		let selected_points = self.selected_points().cloned().collect::<HashSet<_>>();
		let selected_segments = selected_segments(network_interface, self);

		for (&layer, state) in &mut self.selected_shape_state {
			if selection_change == SelectionChange::Clear {
				state.clear_points();
				state.clear_segments();
			}

			let vector_data = network_interface.compute_modified_vector(layer);
			let Some(vector_data) = vector_data else { continue };
			let transform = network_interface.document_metadata().transform_to_viewport_if_feeds(layer, network_interface);

			assert_eq!(vector_data.segment_domain.ids().len(), vector_data.start_point().count());
			assert_eq!(vector_data.segment_domain.ids().len(), vector_data.end_point().count());
			for start in vector_data.start_point() {
				assert!(vector_data.point_domain.ids().contains(&start));
			}
			for end in vector_data.end_point() {
				assert!(vector_data.point_domain.ids().contains(&end));
			}

			let polygon_subpath = if let SelectionShape::Lasso(polygon) = selection_shape {
				if polygon.len() < 2 {
					return;
				}
				let polygon: Subpath<PointId> = Subpath::from_anchors_linear(polygon.to_vec(), true);
				Some(polygon)
			} else {
				None
			};

			// Selection segments
			for (id, bezier, _, _) in vector_data.segment_bezier_iter() {
				if select_segments {
					// Select segments if they lie inside the bounding box or lasso polygon
					let segment_bbox = calculate_bezier_bbox(bezier);
					let bottom_left = transform.transform_point2(segment_bbox[0]);
					let top_right = transform.transform_point2(segment_bbox[1]);

					let select = match selection_shape {
						SelectionShape::Box(quad) => {
							let enclosed = quad[0].min(quad[1]).cmple(bottom_left).all() && quad[0].max(quad[1]).cmpge(top_right).all();
							match selection_mode {
								SelectionMode::Enclosed => enclosed,
								_ => {
									// Check for intersection with the segment
									enclosed || is_intersecting(bezier, quad, transform)
								}
							}
						}
						SelectionShape::Lasso(_) => {
							let polygon = polygon_subpath.as_ref().expect("If `selection_shape` is a polygon then subpath is constructed beforehand.");

							// Sample 10 points on the bezier and check if all or some lie inside the polygon
							let points = bezier.compute_lookup_table(Some(10), None);
							match selection_mode {
								SelectionMode::Enclosed => points.map(|p| transform.transform_point2(p)).all(|p| polygon.contains_point(p)),
								_ => points.map(|p| transform.transform_point2(p)).any(|p| polygon.contains_point(p)),
							}
						}
					};

					if select {
						match selection_change {
							SelectionChange::Shrink => state.deselect_segment(id),
							_ => state.select_segment(id),
						}
					}
				}

				// Selecting handles
				for (position, id) in [(bezier.handle_start(), ManipulatorPointId::PrimaryHandle(id)), (bezier.handle_end(), ManipulatorPointId::EndHandle(id))] {
					let Some(position) = position else { continue };
					let transformed_position = transform.transform_point2(position);

					let select = match selection_shape {
						SelectionShape::Box(quad) => quad[0].min(quad[1]).cmple(transformed_position).all() && quad[0].max(quad[1]).cmpge(transformed_position).all(),
						SelectionShape::Lasso(_) => polygon_subpath
							.as_ref()
							.expect("If `selection_shape` is a polygon then subpath is constructed beforehand.")
							.contains_point(transformed_position),
					};

					if select {
						let is_visible_handle = is_visible_point(id, &vector_data, path_overlay_mode, frontier_handles_info.clone(), selected_segments.clone(), &selected_points);

						if is_visible_handle {
							match selection_change {
								SelectionChange::Shrink => state.deselect_point(id),
								_ => {
									// Select only the handles which are of nonzero length
									if let Some(handle) = id.as_handle() {
										if handle.length(&vector_data) > 0. {
											state.select_point(id)
										}
									}
								}
							}
						}
					}
				}
			}

			// Checking for selection of anchor points
			for (&id, &position) in vector_data.point_domain.ids().iter().zip(vector_data.point_domain.positions()) {
				let transformed_position = transform.transform_point2(position);

				let select = match selection_shape {
					SelectionShape::Box(quad) => quad[0].min(quad[1]).cmple(transformed_position).all() && quad[0].max(quad[1]).cmpge(transformed_position).all(),
					SelectionShape::Lasso(_) => polygon_subpath
						.as_ref()
						.expect("If `selection_shape` is a polygon then subpath is constructed beforehand.")
						.contains_point(transformed_position),
				};

				if select {
					match selection_change {
						SelectionChange::Shrink => state.deselect_point(ManipulatorPointId::Anchor(id)),
						_ => state.select_point(ManipulatorPointId::Anchor(id)),
					}
				}
			}
		}
	}
}
