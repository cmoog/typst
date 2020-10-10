use super::*;

/// A node that arranges its children into a paragraph.
///
/// Boxes are laid out along the cross axis as long as they fit into a line.
/// When necessary, a line break is inserted and the new line is offset along
/// the main axis by the height of the previous line plus extra line spacing.
#[derive(Debug, Clone, PartialEq)]
pub struct Par {
    pub dirs: Gen2<Dir>,
    pub line_spacing: f64,
    pub children: Vec<LayoutNode>,
    pub aligns: Gen2<GenAlign>,
    pub expand: Spec2<bool>,
}

#[async_trait(?Send)]
impl Layout for Par {
    async fn layout(
        &self,
        ctx: &mut LayoutContext,
        constraints: LayoutConstraints,
    ) -> Vec<LayoutItem> {
        let mut layouter = LineLayouter::new(LineContext {
            dirs: self.dirs,
            spaces: constraints.spaces,
            repeat: constraints.repeat,
            line_spacing: self.line_spacing,
            expand: self.expand,
        });

        for child in &self.children {
            let items = child
                .layout(ctx, LayoutConstraints {
                    spaces: layouter.remaining(),
                    repeat: constraints.repeat,
                })
                .await;

            for item in items {
                match item {
                    LayoutItem::Spacing(amount) => layouter.push_spacing(amount),
                    LayoutItem::Box(boxed, aligns) => layouter.push_box(boxed, aligns),
                }
            }
        }

        layouter
            .finish()
            .into_iter()
            .map(|boxed| LayoutItem::Box(boxed, self.aligns))
            .collect()
    }
}

impl From<Par> for LayoutNode {
    fn from(par: Par) -> Self {
        Self::dynamic(par)
    }
}

/// Performs the line layouting.
struct LineLayouter {
    /// The context used for line layouting.
    ctx: LineContext,
    /// The underlying layouter that stacks the finished lines.
    stack: StackLayouter,
    /// The in-progress line.
    run: LineRun,
}

/// The context for line layouting.
#[derive(Debug, Clone)]
struct LineContext {
    /// The layout directions.
    dirs: Gen2<Dir>,
    /// The spaces to layout into.
    spaces: Vec<LayoutSpace>,
    /// Whether to spill over into copies of the last space or finish layouting
    /// when the last space is used up.
    repeat: bool,
    /// The spacing to be inserted between each pair of lines.
    line_spacing: f64,
    /// Whether to expand the size of the resulting layout to the full size of
    /// this space or to shrink it to fit the content.
    expand: Spec2<bool>,
}

impl LineLayouter {
    /// Create a new line layouter.
    fn new(ctx: LineContext) -> Self {
        Self {
            stack: StackLayouter::new(StackContext {
                spaces: ctx.spaces.clone(),
                dirs: ctx.dirs,
                repeat: ctx.repeat,
                expand: ctx.expand,
            }),
            ctx,
            run: LineRun::new(),
        }
    }

    /// Add a layout.
    fn push_box(&mut self, layout: BoxLayout, aligns: Gen2<GenAlign>) {
        let dirs = self.ctx.dirs;
        if let Some(prev) = self.run.aligns {
            if aligns.main != prev.main {
                // TODO: Issue warning for non-fitting alignment in
                // non-repeating context.
                let fitting = aligns.main >= self.stack.space.allowed_align;
                if !fitting && self.ctx.repeat {
                    self.finish_space(true);
                } else {
                    self.finish_line();
                }
            } else if aligns.cross < prev.cross {
                self.finish_line();
            } else if aligns.cross > prev.cross {
                let usable = self.stack.usable().get(dirs.cross.axis());

                let mut rest_run = LineRun::new();
                rest_run.size.main = self.run.size.main;

                // FIXME: Alignment in non-expanding parent.
                rest_run.usable = Some(match aligns.cross {
                    GenAlign::Start => unreachable!("start > x"),
                    GenAlign::Center => usable - 2.0 * self.run.size.cross,
                    GenAlign::End => usable - self.run.size.cross,
                });

                self.finish_line();

                // Move back up in the stack layouter.
                self.stack.push_spacing(-rest_run.size.main - self.ctx.line_spacing);
                self.run = rest_run;
            }
        }

        let size = layout.size.switch(dirs);
        let usable = self.usable();

        if usable.main < size.main || usable.cross < size.cross {
            if !self.line_is_empty() {
                self.finish_line();
            }

            // TODO: Issue warning about overflow if there is overflow.
            let usable = self.usable();
            if usable.main < size.main || usable.cross < size.cross {
                self.stack.skip_to_fitting_space(layout.size);
            }
        }

        self.run.aligns = Some(aligns);
        self.run.layouts.push((self.run.size.cross, layout));

        self.run.size.cross += size.cross;
        self.run.size.main = self.run.size.main.max(size.main);
    }

    /// Add spacing to the line.
    fn push_spacing(&mut self, mut spacing: f64) {
        spacing = spacing.min(self.usable().cross);
        self.run.size.cross += spacing;
    }

    /// The remaining usable size of the line.
    ///
    /// This specifies how much more would fit before a line break would be
    /// needed.
    fn usable(&self) -> Gen2<f64> {
        // The base is the usable space of the stack layouter.
        let mut usable = self.stack.usable().switch(self.ctx.dirs);

        // If there was another run already, override the stack's size.
        if let Some(cross) = self.run.usable {
            usable.cross = cross;
        }

        usable.cross -= self.run.size.cross;
        usable
    }

    /// The remaining inner spaces. If something is laid out into these spaces,
    /// it will fit into this layouter's underlying stack.
    fn remaining(&self) -> Vec<LayoutSpace> {
        let mut spaces = self.stack.remaining();
        *spaces[0].size.get_mut(self.ctx.dirs.main.axis()) -= self.run.size.main;
        spaces
    }

    /// Whether the currently set line is empty.
    fn line_is_empty(&self) -> bool {
        self.run.size == Gen2::ZERO && self.run.layouts.is_empty()
    }

    /// Finish everything up and return the final collection of boxes.
    fn finish(mut self) -> Vec<BoxLayout> {
        self.finish_line_if_not_empty();
        self.stack.finish()
    }

    /// Finish the active space and start a new one.
    ///
    /// At the top level, this is a page break.
    fn finish_space(&mut self, hard: bool) {
        self.finish_line_if_not_empty();
        self.stack.finish_space(hard)
    }

    /// Finish the active line and start a new one.
    fn finish_line(&mut self) {
        let dirs = self.ctx.dirs;

        let mut layout = BoxLayout::new(self.run.size.switch(dirs).to_size());
        let aligns = self.run.aligns.unwrap_or_default();

        let children = std::mem::take(&mut self.run.layouts);
        for (offset, child) in children {
            let cross = if dirs.cross.is_positive() {
                offset
            } else {
                self.run.size.cross - offset - child.size.get(dirs.cross.axis())
            };

            let pos = Gen2::new(0.0, cross).switch(dirs).to_point();
            layout.push_layout(pos, child);
        }

        self.stack.push_box(layout, aligns);
        self.stack.push_spacing(self.ctx.line_spacing);
        self.run = LineRun::new();
    }

    fn finish_line_if_not_empty(&mut self) {
        if !self.line_is_empty() {
            self.finish_line()
        }
    }
}

/// A sequence of boxes with the same alignment. A real line can consist of
/// multiple runs with different alignments.
struct LineRun {
    /// The so-far accumulated items of the run.
    layouts: Vec<(f64, BoxLayout)>,
    /// The summed width and maximal height of the run.
    size: Gen2<f64>,
    /// The alignment of all layouts in the line.
    ///
    /// When a new run is created the alignment is yet to be determined and
    /// `None` as such. Once a layout is added, its alignment decides the
    /// alignment for the whole run.
    aligns: Option<Gen2<GenAlign>>,
    /// The amount of cross-space left by another run on the same line or `None`
    /// if this is the only run so far.
    usable: Option<f64>,
}

impl LineRun {
    fn new() -> Self {
        Self {
            layouts: vec![],
            size: Gen2::ZERO,
            aligns: None,
            usable: None,
        }
    }
}

/// Performs the stack layouting.
pub(super) struct StackLayouter {
    /// The context used for stack layouting.
    pub ctx: StackContext,
    /// The finished layouts.
    pub layouts: Vec<BoxLayout>,
    /// The in-progress space.
    pub space: Space,
}

/// The context for stack layouting.
#[derive(Debug, Clone)]
pub(super) struct StackContext {
    /// The layouting directions.
    pub dirs: Gen2<Dir>,
    /// The spaces to layout into.
    pub spaces: Vec<LayoutSpace>,
    /// Whether to spill over into copies of the last space or finish layouting
    /// when the last space is used up.
    pub repeat: bool,
    /// Whether to expand the size of the resulting layout to the full size of
    /// this space or to shrink it to fit the content.
    pub expand: Spec2<bool>,
}

impl StackLayouter {
    /// Create a new stack layouter.
    pub fn new(ctx: StackContext) -> Self {
        let space = ctx.spaces[0];
        Self {
            ctx,
            layouts: vec![],
            space: Space::new(0, true, space.size),
        }
    }

    /// Add a layout to the stack.
    pub fn push_box(&mut self, layout: BoxLayout, aligns: Gen2<GenAlign>) {
        // If the alignment cannot be fitted in this space, finish it.
        //
        // TODO: Issue warning for non-fitting alignment in non-repeating
        //       context.
        if aligns.main < self.space.allowed_align && self.ctx.repeat {
            self.finish_space(true);
        }

        // TODO: Issue warning about overflow if there is overflow in a
        //       non-repeating context.
        if !self.space.usable.fits(layout.size) && self.ctx.repeat {
            self.skip_to_fitting_space(layout.size);
        }

        // Change the usable space and size of the space.
        self.update_metrics(layout.size.switch(self.ctx.dirs));

        // Add the box to the vector and remember that spacings are allowed
        // again.
        self.space.layouts.push((layout, aligns));
        self.space.allowed_align = aligns.main;
    }

    /// Add spacing to the stack.
    pub fn push_spacing(&mut self, mut spacing: f64) {
        // Reduce the spacing such that it definitely fits.
        let axis = self.ctx.dirs.main.axis();
        spacing = spacing.min(self.space.usable.get(axis));

        let size = Gen2::new(spacing, 0.0);
        self.update_metrics(size);
        self.space.layouts.push((
            BoxLayout::new(size.switch(self.ctx.dirs).to_size()),
            Gen2::default(),
        ));
    }

    fn update_metrics(&mut self, added: Gen2<f64>) {
        let mut used = self.space.used.switch(self.ctx.dirs);
        used.cross = used.cross.max(added.cross);
        used.main += added.main;
        self.space.used = used.switch(self.ctx.dirs).to_size();
        *self.space.usable.get_mut(self.ctx.dirs.main.axis()) -= added.main;
    }

    /// Move to the first space that can fit the given size or do nothing
    /// if no space is capable of that.
    pub fn skip_to_fitting_space(&mut self, size: Size) {
        let start = self.next_space();
        for (index, space) in self.ctx.spaces[start ..].iter().enumerate() {
            if space.size.fits(size) {
                self.finish_space(true);
                self.start_space(start + index, true);
                break;
            }
        }
    }

    /// The remaining inner spaces. If something is laid out into these spaces,
    /// it will fit into this stack.
    pub fn remaining(&self) -> Vec<LayoutSpace> {
        let mut spaces = vec![LayoutSpace {
            base: self.space.size,
            size: self.space.usable,
        }];

        spaces.extend(&self.ctx.spaces[self.next_space() ..]);
        spaces
    }

    /// The remaining usable size.
    pub fn usable(&self) -> Size {
        self.space.usable
    }

    /// Whether the current layout space is empty.
    pub fn space_is_empty(&self) -> bool {
        self.space.used == Size::ZERO && self.space.layouts.is_empty()
    }

    /// Finish everything up and return the final collection of boxes.
    pub fn finish(mut self) -> Vec<BoxLayout> {
        if self.space.hard || !self.space_is_empty() {
            self.finish_space(false);
        }
        self.layouts
    }

    /// Finish active current space and start a new one.
    pub fn finish_space(&mut self, hard: bool) {
        let dirs = self.ctx.dirs;

        // ------------------------------------------------------------------ //
        // Step 1: Determine the full size of the space.
        // (Mostly done already while collecting the boxes, but here we
        //  expand if necessary.)

        let space = self.ctx.spaces[self.space.index];
        let layout_size = {
            let mut used_size = self.space.used;
            if self.ctx.expand.horizontal {
                used_size.width = space.size.width;
            }
            if self.ctx.expand.vertical {
                used_size.height = space.size.height;
            }
            used_size
        };

        let mut layout = BoxLayout::new(layout_size);

        // ------------------------------------------------------------------ //
        // Step 2: Forward pass. Create a bounding box for each layout in which
        // it will be aligned. Then, go forwards through the boxes and remove
        // what is taken by previous layouts from the following layouts.

        let mut bounds = vec![];
        let mut bound = Rect {
            x0: 0.0,
            y0: 0.0,
            x1: layout_size.width,
            y1: layout_size.height,
        };

        for (layout, _) in &self.space.layouts {
            // First, store the bounds calculated so far (which were reduced
            // by the predecessors of this layout) as the initial bounding box
            // of this layout.
            bounds.push(bound);

            // Then, reduce the bounding box for the following layouts. This
            // layout uses up space from the origin to the end. Thus, it reduces
            // the usable space for following layouts at its origin by its
            // main-axis extent.
            *bound.get_mut(dirs.main.start()) +=
                dirs.main.factor() * layout.size.get(dirs.main.axis());
        }

        // ------------------------------------------------------------------ //
        // Step 3: Backward pass. Reduce the bounding boxes from the previous
        // layouts by what is taken by the following ones.

        let mut main_extent = 0.0;
        for (child, bound) in self.space.layouts.iter().zip(&mut bounds).rev() {
            let (layout, _) = child;

            // Reduce the bounding box of this layout by the following one's
            // main-axis extents.
            *bound.get_mut(dirs.main.end()) -= dirs.main.factor() * main_extent;

            // And then, include this layout's main-axis extent.
            main_extent += layout.size.get(dirs.main.axis());
        }

        // ------------------------------------------------------------------ //
        // Step 4: Align each layout in its bounding box and collect everything
        // into a single finished layout.

        let children = std::mem::take(&mut self.space.layouts);
        for ((child, aligns), bound) in children.into_iter().zip(bounds) {
            // Align the child in its own bounds.
            let local =
                bound.size().anchor(dirs, aligns) - child.size.anchor(dirs, aligns);

            // Make the local position in the bounds global.
            let pos = bound.origin() + local;
            layout.push_layout(pos, child);
        }

        self.layouts.push(layout);

        // ------------------------------------------------------------------ //
        // Step 5: Start the next space.

        self.start_space(self.next_space(), hard)
    }

    fn start_space(&mut self, index: usize, hard: bool) {
        let space = self.ctx.spaces[index];
        self.space = Space::new(index, hard, space.size);
    }

    fn next_space(&self) -> usize {
        (self.space.index + 1).min(self.ctx.spaces.len() - 1)
    }
}

/// A layout space composed of subspaces which can have different directions and
/// alignments.
#[derive(Debug)]
pub(super) struct Space {
    /// The index of this space in `ctx.spaces`.
    index: usize,
    /// Whether to include a layout for this space even if it would be empty.
    hard: bool,
    /// The so-far accumulated layouts.
    layouts: Vec<(BoxLayout, Gen2<GenAlign>)>,
    /// The full size of this space.
    size: Size,
    /// The used size of this space.
    used: Size,
    /// The remaining space.
    usable: Size,
    /// Which alignments for new boxes are still allowed.
    pub(super) allowed_align: GenAlign,
}

impl Space {
    fn new(index: usize, hard: bool, size: Size) -> Self {
        Self {
            index,
            hard,
            layouts: vec![],
            size,
            used: Size::ZERO,
            usable: size,
            allowed_align: GenAlign::Start,
        }
    }
}
