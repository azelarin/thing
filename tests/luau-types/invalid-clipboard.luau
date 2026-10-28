--!strict

services.clipboard.set_text(42)
services.clipboard.clear("unexpected")
services.clipboard.text:set("mutable")

return ui.panel {}
