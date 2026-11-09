use onyx_soul::{
    Attr, AttrRegistry, ConceptId, OnyxError, Value,
    event::{IngressEvent, OnyxEvent},
    system::{Ctx, OnyxSystem},
};

pub struct BootSystem {
    name_attr: Attr,
    clearance_attr: Attr,
    player: Option<ConceptId>,
}

impl BootSystem {
    pub fn new() -> Self {
        Self {
            name_attr: Attr::from_raw(0),
            clearance_attr: Attr::from_raw(0),
            player: None,
        }
    }
}

impl OnyxSystem for BootSystem {
    fn name(&self) -> &str {
        "boot"
    }

    fn init(&mut self, attrs: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
        self.name_attr = attrs.attr("name");
        self.clearance_attr = attrs.attr("clearance_level");
        Ok(())
    }

    fn run(&mut self, ctx: &mut Ctx) {
        if self.player.is_none() {
            let existing = ctx.graph.query(&[self.name_attr, self.clearance_attr]);
            if let Some(&id) = existing.first() {
                self.player = Some(id);
                let clearance = ctx.graph.get(id, self.clearance_attr);
                println!("[BOOT] Reconectado a {:?}. Clearance: {:?}", id, clearance);
            } else {
                let id = ctx.graph.create();
                ctx.graph
                    .set(id, self.name_attr, Value::Text("engineer_2233".into()));
                ctx.graph.set(id, self.clearance_attr, Value::Int(0));
                self.player = Some(id);
                println!("[BOOT] System online. Awaiting 'agree'...");
            }
        }

        for event in ctx.inbox {
            if let OnyxEvent::Ingress(IngressEvent::Command(cmd)) = event {
                match cmd.trim().to_lowercase().as_str() {
                    "agree" => {
                        if let Some(id) = self.player {
                            ctx.graph.set(id, self.clearance_attr, Value::Int(1));
                            println!("[BOOT] NDA accepted. Clearance = 1");
                        }
                    }
                    "status" => {
                        if let Some(id) = self.player {
                            let clearance = ctx.graph.get(id, self.clearance_attr);
                            println!("[BOOT] {:?}, Clearance: {:?}", id, clearance);
                        }
                    }
                    _ => println!("[BOOT] Unknown: {}", cmd),
                }
            }
        }
    }
}
