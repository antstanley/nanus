"""Original dwarf jay illustration, drawn using vectorcraft-cli's stdio MCP server."""
import base64
import json
from pathlib import Path
import subprocess

HERE = Path(__file__).resolve().parent
STEPS = []

def command(command_id, **params):
    STEPS.append({"id": command_id, "params": params})

def path(name, d, fill, stroke="none", width=1, **options):
    command("draw_path", name=name, d=d, fill=fill, stroke=stroke,
            strokeWidth=width, cap="round", join="round", **options)

def ellipse(name, x, y, w, h, fill, **options):
    command("draw_shape", shape="ellipse", name=name, x=x, y=y,
            width=w, height=h, fill=fill, stroke="none", **options)

def line(name, d, color, width=1):
    path(name, d, "none", color, width)

def layer(name):
    command("layer.new", name=name)

command("file.new", name="Dwarf jay - Cyanolyca nanus", width=960, height=960,
        units="Pixels", colorMode="rgb", backgroundContents="transparent")
layer("01 - Paper and cloud forest")
command("draw_shape", shape="rectangle", name="Warm ivory paper", x=0, y=0,
        width=960, height=960, fill="#f5f1e7", stroke="none")
ellipse("Quiet forest halo", 197, 160, 586, 586, "#e3e8db")
path("Distant cloud bank", "M209 537 C265 501 278 529 326 492 C368 460 409 481 455 450 C501 420 559 451 602 420 C649 390 715 422 770 455 C790 532 779 605 739 660 C643 720 516 755 390 724 C289 701 231 635 209 537 Z", "#d8e0d2")
path("Low mist", "M222 604 C327 557 400 591 475 563 C552 534 646 555 771 511 C771 620 710 704 616 731 C450 781 289 710 222 604 Z", "#d1dbcd")
line("High pine twig", "M723 316 C750 266 779 223 810 191", "#aebfa8", 3)
for i,(x,y) in enumerate([(739,287),(750,269),(763,249),(778,229),(793,210)]):
    line(f"Pine needles {i}", f"M{x} {y} l-36 -41 M{x} {y} l-18 -49 M{x} {y} l7 -44 M{x} {y} l43 -13 M{x} {y} l49 4", "#b4c3ac", 1.5)
line("Lower woodland twig", "M183 708 C198 667 196 620 176 579", "#b1c0a8", 2.5)
for i,(x,y) in enumerate([(194,669),(193,647),(187,624),(179,601)]):
    path(f"Understory leaf left {i}", f"M{x} {y} C{x-30} {y-2} {x-43} {y-21} {x-43} {y-36} C{x-18} {y-31} {x-9} {y-15} {x} {y} Z", "#bcc9b1")
    path(f"Understory leaf right {i}", f"M{x} {y} C{x+5} {y-22} {x+22} {y-37} {x+34} {y-37} C{x+34} {y-16} {x+16} {y-4} {x} {y} Z", "#bcc9b1")

layer("02 - Moss-covered oak perch")
path("Oak branch silhouette", "M125 693 C277 661 415 638 564 620 C669 607 743 580 828 546 L838 563 C751 609 668 633 568 645 C410 664 278 690 132 713 Z", "#665e4d")
path("Warm upper bark", "M125 693 C287 660 425 636 564 620 C668 607 745 579 828 546 L829 553 C729 596 665 620 560 630 C400 650 256 682 129 702 Z", "#94856a")
line("Bark fissures", "M161 701 C279 677 342 669 413 657 M551 637 C625 626 677 616 722 599 M307 674 l35 -2 M750 587 l39 -17", "#4e5145", 2)
path("Upturned fork", "M711 596 C738 571 751 542 755 510 L762 510 C762 547 747 578 733 593 Z", "#776b54")
line("Fork highlight", "M726 586 Q753 550 758 516", "#a09072", 2)
path("Velvet moss", "M148 689 Q162 680 174 685 Q185 675 196 681 Q210 668 222 676 Q234 665 245 672 Q258 660 272 669 L312 664 Q327 650 342 659 Q356 644 367 652 Q383 637 394 648 L445 638 Q459 625 469 634 L491 630 L493 638 C356 658 264 680 150 699 Z", "#7b8c59")
line("Moss tips", "M173 686 l-2 -9 M198 680 l5 -11 M236 674 l-2 -9 M269 669 l4 -10 M330 657 l-3 -8 M363 651 l4 -10 M392 648 l3 -10 M458 634 l-2 -7", "#9eaa75", 2.5)
for i,(x,y) in enumerate([(290,676),(303,673),(603,626),(615,625),(625,621),(689,608)]):
    ellipse(f"Lichen {i}",x,y,9,4,"#b8b99a")

layer("03 - Tail and far leg")
path("Far leg", "M503 557 C504 580 510 602 522 626 L533 627 C524 598 519 575 520 553 Z", "#343e45")
line("Far toes", "M527 623 C518 622 511 625 508 635 M527 624 C539 619 548 623 550 630 M527 624 C534 626 537 631 536 637", "#343e45", 5)
path("Tail silhouette", "M548 478 C593 510 661 570 743 639 C759 652 775 663 778 675 C761 692 739 692 723 677 C669 628 606 592 552 548 Z", "#344c69")
path("Tail outer feather", "M572 509 C624 548 691 610 768 670 Q766 681 751 683 C684 626 624 591 565 542 Z", "#4d6f93")
path("Tail middle feather", "M563 505 C620 551 676 602 747 673 Q742 683 730 675 C664 621 605 580 553 545 Z", "#5c7fa1")
path("Tail inner feather", "M550 510 C602 552 652 601 717 660 Q720 671 708 666 C641 615 593 582 543 545 Z", "#6c8bab")
line("Tail shafts", "M581 534 Q665 605 757 676 M567 539 Q647 609 734 673 M559 544 L704 660", "#2f4966", 1.6)
line("Tail blue edge", "M610 547 Q683 610 768 671", "#8da9bf", 1.5)

layer("04 - Body and pale throat")
path("Compact body", "M420 344 C459 355 473 377 513 389 C555 402 591 446 602 488 C612 516 604 540 579 559 C548 587 508 596 460 583 C416 572 383 541 369 504 C358 475 355 448 345 420 C336 397 332 380 340 358 Z", "#718ba5")
path("Underbelly shadow", "M373 474 C402 527 454 556 499 554 C538 553 571 531 600 501 C606 532 589 552 566 568 C529 594 489 593 454 581 C411 566 382 529 373 474 Z", "#5e778f")
path("Breast soft light", "M351 404 C367 397 399 396 425 416 C449 436 449 481 470 514 C488 543 516 553 543 553 C506 575 469 566 439 547 C397 521 372 474 351 404 Z", "#91a7b8")
path("Breast middle plane", "M367 431 C387 458 389 486 408 510 C426 532 448 543 466 548 C422 544 386 503 376 471 Z", "#a2b4c0")
path("Pale lavender-blue throat", "M334 376 C352 371 384 375 414 389 C423 403 428 424 443 442 C426 439 414 434 402 424 L404 434 L389 420 L389 429 L376 413 L374 423 C351 409 338 394 334 376 Z", "#c0ced7")
path("Throat highlight", "M337 379 C357 378 373 385 384 394 C370 397 354 393 342 389 Z", "#d9e0e1")
for i,(d,c) in enumerate([
    ("M383 441 q5 16 15 22", "#b8c6cd"),
    ("M394 469 q5 15 16 22", "#bdc8ce"),
    ("M413 497 q7 14 18 18", "#b8c6cd"),
    ("M435 524 q8 12 20 14", "#aebfc9"),
    ("M453 561 l11 6 M470 566 l12 5 M489 569 l11 2", "#91a5b3"),
    ("M377 467 l6 15 M388 499 l7 12", "#7f99ae")]):
    line(f"Breast feather contour {i}",d,c,1.8)

layer("05 - Folded wing and flight feathers")
path("Wing dark silhouette", "M449 404 C489 382 526 398 550 423 C576 450 577 486 600 524 L624 565 C602 575 569 563 544 545 C503 516 467 479 449 438 C444 425 443 412 449 404 Z", "#354e6b")
path("Outer flight feather", "M485 421 C528 434 553 475 574 506 L619 561 Q610 568 599 562 C550 527 509 485 477 447 Z", "#52779b")
path("Second flight feather", "M474 423 C513 440 540 481 561 505 L604 553 Q597 561 585 551 C540 517 500 477 468 445 Z", "#5e83a6")
path("Third flight feather", "M464 422 C493 441 521 475 543 499 L585 542 Q576 550 565 539 C526 508 494 477 460 443 Z", "#6a8daf")
path("Fourth flight feather", "M458 426 C484 447 502 470 527 493 L565 528 Q554 534 545 526 C511 502 481 470 455 445 Z", "#7798b6")
line("Flight feather separations", "M486 443 C530 490 575 538 611 559 M476 446 C516 491 555 528 594 551 M466 447 C501 486 538 520 574 541 M459 449 Q502 494 554 527", "#324f6d", 1.8)
line("Flight feather fine shafts", "M510 462 L601 552 M502 469 L581 538 M492 472 L561 523", "#90abc2", 1.1)
path("Blue shoulder mantle", "M444 408 C463 389 493 392 516 405 C537 418 551 443 554 461 C539 462 526 453 514 445 L519 455 C504 453 493 442 484 434 L489 446 C465 436 447 425 444 408 Z", "#7497b6")
path("Shoulder light", "M450 408 C469 395 492 398 507 407 C490 406 476 410 468 419 Z", "#98b4ca")
path("Greater coverts scallops", "M464 420 Q477 414 490 423 L531 469 Q526 478 517 470 L484 441 Q493 460 513 480 Q508 486 500 479 L468 449 Q475 465 492 482 Q485 486 478 478 L456 442 Z", "#688bad")
line("Covert feather accents", "M468 425 Q481 429 490 441 M480 421 Q495 429 506 443 M494 420 Q511 430 520 446 M508 425 Q525 435 533 452", "#a0b9cc", 1.5)
line("Wing top blue rim", "M451 405 C478 389 507 400 524 414", "#aec3d0", 2)

layer("06 - Crown, black mask and eye")
path("Rounded uncrested head", "M329 349 C325 325 337 305 357 294 C382 279 413 286 434 303 C451 318 459 344 451 365 C446 380 433 394 414 400 C389 393 367 384 347 378 Z", "#6488aa")
path("Crown illuminated plane", "M330 337 C328 317 344 301 363 294 C386 286 413 293 427 306 C393 295 364 308 350 329 Z", "#8caec6")
path("Crown blue midtone", "M351 318 C372 300 399 298 421 309 C439 319 447 335 448 352 C424 330 396 318 368 327 Z", "#7097b9")
path("Nape feathers", "M427 313 C449 327 459 352 450 374 L435 385 L437 370 L426 378 L429 361 L416 366 L424 349 Z", "#547596")
path("Black facial mask", "M329 349 C344 336 359 335 375 341 C395 345 416 353 439 352 C440 372 429 391 411 399 C397 393 382 391 369 391 C350 393 338 383 332 371 Z", "#273441")
path("Mask lower blue reflection", "M348 378 C368 388 388 379 411 387 L424 382 Q420 393 411 396 C390 388 368 399 348 378 Z", "#3c4d5b")
path("Distinct pale-blue brow", "M328 345 C339 331 354 328 369 331 C394 334 411 346 439 347 L440 353 C412 353 392 343 369 338 C352 334 340 339 329 351 Z", "#c0d5df")
line("Blue eyebrow upper edge", "M333 339 C349 325 367 329 380 333", "#dbe4e5", 1.3)
path("Upper bill", "M335 350 C317 350 292 356 274 370 C292 367 316 368 337 365 C343 362 340 355 335 350 Z", "#25323a")
path("Lower bill", "M276 370 C296 370 317 374 334 373 L341 363 C318 367 296 366 276 370 Z", "#19272f")
path("Bill cool reflection", "M286 364 Q311 353 331 354 L334 357 Q310 357 286 364 Z", "#617783")
line("Bill gape", "M284 369 Q316 365 335 365", "#111f28", 1.2)
ellipse("Nostril",321,355,5,2.8,"#14232b")
ellipse("Eye socket",358,343,21,21,"#192731")
ellipse("Red-brown iris",362,346,14,14,"#955e42")
ellipse("Iris sunward rim",363,347,10.5,10.5,"#b68155")
ellipse("Dark pupil",365,347,8.5,10,"#101d26")
ellipse("Eye catchlight",365,347,3.5,3.5,"#f9f6e9")
ellipse("Tiny eye reflection",371,354,1.5,1.5,"#d2dce0")
line("Lower eyelid", "M362 361 Q370 365 376 359", "#698296", 1.2)
line("Cheek feather glints", "M387 365 l9 4 M392 375 l12 3 M412 363 l9 1", "#4c6172", 1.2)
line("Crown fine feather marks", "M348 317 l9 -7 M361 311 l9 -5 M377 307 l9 -2 M396 309 l8 2 M416 319 l6 5", "#b1c9d6", 1.4)

layer("07 - Near foot gripping branch")
path("Near leg", "M453 563 C456 584 453 607 457 631 L465 638 L474 632 C465 611 468 586 470 569 Z", "#38464e")
line("Near leg highlight", "M460 579 Q459 610 465 629", "#7e8b8d", 2)
line("Near long front toe", "M465 634 C475 631 488 632 494 638 Q498 641 494 648", "#2e3b42", 5)
line("Near second toe", "M465 635 C472 636 478 641 478 649", "#35434a", 4.5)
line("Rear toe", "M462 634 C451 633 441 637 439 644", "#35434a", 4.5)
line("Toe highlights", "M471 634 Q485 632 492 638 M448 636 l10 -1", "#84908d", 1.5)
path("Front claw", "M493 644 Q500 649 490 654 L492 648 Z", "#c1bbaa")
path("Middle claw", "M476 647 Q482 653 475 656 L476 650 Z", "#b9b5a5")
path("Back claw", "M438 641 Q434 648 442 649 L441 645 Z", "#b9b5a5")
line("Leg scales", "M457 605 l7 1 M458 613 l6 1 M459 621 l7 1", "#9ba3a0", 1)

layer("08 - Natural history caption")
line("Top register rule", "M76 86 H134", "#839381", 2)
command("text.create",text="CLOUD FOREST  /  MEXICO",x=150,y=91,size=12,font="Helvetica",color="#647360")
command("text.create",text="CORVIDAE     /     01",x=884,y=91,justify="right",size=11,font="Helvetica",color="#647360")
line("Caption divider", "M90 782 H870", "#b8bfae", 1)
command("text.create",text="Dwarf jay",x=90,y=843,size=48,font="Baskerville",color="#2d4549")
command("text.create",text="Cyanolyca nanus",x=93,y=875,size=20,font="Baskerville",color="#637b7e")
command("text.create",text="THE SMALLEST CORVID",x=868,y=823,size=12,font="Helvetica",color="#415b5a",justify="right")
command("text.create",text="20–23 cm  ·  Pine–oak highlands",x=868,y=848,size=13,font="Helvetica",color="#637b72",justify="right")
command("text.create",text="Endemic to the mountains of southern Mexico",x=868,y=873,size=11,font="Helvetica",color="#7c8879",justify="right")

def main():
    proc = subprocess.Popen(["vectorcraft-cli","mcp","--headless",
        "--automation-read-root",str(HERE),"--automation-write-root",str(HERE)],
        stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True)
    seq = 0
    transcript = []
    def request(method,params):
        nonlocal seq
        seq += 1
        proc.stdin.write(json.dumps({"jsonrpc":"2.0","id":seq,"method":method,"params":params})+"\n")
        proc.stdin.flush()
        while True:
            response = json.loads(proc.stdout.readline())
            if response.get("id") == seq:
                break
        if "error" in response:
            raise RuntimeError(response)
        result = response["result"]
        if result.get("isError"):
            raise RuntimeError(result)
        return result
    def call(name,**arguments):
        result = request("tools/call",{"name":name,"arguments":arguments})
        texts = [c["text"] for c in result.get("content",[]) if c["type"]=="text"]
        transcript.append({"tool":name,"text":texts})
        for t in texts:
            print(t[:1800])
        return result
    try:
        request("initialize",{"protocolVersion":"2025-06-18","capabilities":{},
            "clientInfo":{"name":"nanus-illustration","version":"1"}})
        proc.stdin.write('{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
        proc.stdin.flush()
        call("command_batch",steps=STEPS,atomic=True,label="Draw dwarf jay natural-history plate")
        call("export",path=str(HERE/"dwarf-jay.svg"),format="svg",outlineText=True,
            options={"decimals":3})
        call("export",path=str(HERE/"dwarf-jay.png"),format="png",scale=1)
        call("save_file",path=str(HERE/"dwarf-jay.vectorcraft"))
        call("inspect_document",depth=0)
        preview=call("render_preview",max_side=960)
        for c in preview.get("content",[]):
            if c["type"]=="image":
                (HERE/"mcp-preview.png").write_bytes(base64.b64decode(c["data"]))
    finally:
        (HERE/"mcp-log.json").write_text(json.dumps(transcript,indent=2))
        proc.stdin.close()
        proc.wait()

if __name__=="__main__":
    main()
