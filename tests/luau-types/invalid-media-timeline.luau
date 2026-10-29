--!strict

services.media.seek('later')
services.media.set_position()
local _length: number = services.media.length_ms:get()
local _position: number = services.media.position()

return ui.panel {}
