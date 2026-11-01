--!strict

services.clipboard.set_image("/path/to/file.png")
local status: number = services.clipboard.image_status:get()
services.clipboard.read_image(true)

return ui.panel {}
