#!/usr/bin/env python3
"""Seed the migrated wger 2.7 database for the contract harness.

Writes /tmp/wger-harness-ids.json and /tmp/wger-harness.env (JWT keys).
Neither file is part of the repo.
"""

import base64
import json
import os
import sys
from datetime import date, datetime
from decimal import Decimal
from pathlib import Path

os.environ.setdefault("DJANGO_SETTINGS_MODULE", "settings.main")
os.environ.setdefault("DJANGO_DEBUG", "True")
os.environ.setdefault("DJANGO_DB_ENGINE", "django.db.backends.postgresql")
os.environ.setdefault("DJANGO_DB_DATABASE", "wger")
os.environ.setdefault("DJANGO_DB_USER", "wger")
os.environ.setdefault("DJANGO_DB_PASSWORD", "wger")
os.environ.setdefault("DJANGO_DB_HOST", "127.0.0.1")
os.environ.setdefault("DJANGO_DB_PORT", "5432")
os.environ.setdefault("SECRET_KEY", "contract-harness")
os.environ.setdefault("TIME_ZONE", "America/Santiago")
os.environ.setdefault("SITE_URL", "http://127.0.0.1:8000")
os.environ.setdefault("DJANGO_MEDIA_ROOT", "/tmp/wger-media")
os.environ.setdefault("DJANGO_STATIC_ROOT", "/tmp/wger-static")

ROOT = Path("/tmp/wger-src/server-2.7")
sys.path.insert(0, str(ROOT))

import django  # noqa: E402

django.setup()

from django.contrib.auth.models import User  # noqa: E402
from django.core.files.base import ContentFile  # noqa: E402
from django.utils import timezone  # noqa: E402
from rest_framework.authtoken.models import Token  # noqa: E402

from allauth.account.models import EmailAddress  # noqa: E402
from wger.core.models import Language, License  # noqa: E402
from wger.exercises.models import (  # noqa: E402
    Alias,
    Equipment,
    Exercise,
    ExerciseCategory,
    ExerciseComment,
    ExerciseImage,
    Muscle,
    Translation,
)
from wger.gallery.models import Image as GalleryImage  # noqa: E402
from wger.gym.models import Gym  # noqa: E402
from wger.manager.models import (  # noqa: E402
    Day,
    Label,
    MaxRepetitionsConfig,
    MaxRestConfig,
    MaxRiRConfig,
    MaxSetsConfig,
    MaxWeightConfig,
    RepetitionsConfig,
    RestConfig,
    RiRConfig,
    Routine,
    SetsConfig,
    Slot,
    SlotEntry,
    WeightConfig,
    WorkoutLog,
    WorkoutSession,
)
from wger.measurements.models import Category, Measurement  # noqa: E402
from wger.nutrition.models import (  # noqa: E402
    Ingredient,
    IngredientWeightUnit,
    LogItem,
    Meal,
    MealItem,
    NutritionPlan,
)

PBKDF2 = "pbkdf2_sha256$1200000$abcxyz$Uqzl0m+myx+77kJdT0HsQQEd+KOhzyyCyLwUwAJJVDQ="
TOKEN = "0123456789abcdef0123456789abcdef01234567"
PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
)
DESC = "Lower the bar to the chest, then press it back to lockout."


def keys():
    from cryptography.hazmat.primitives.asymmetric import rsa
    from jwt.algorithms import RSAAlgorithm

    rsa_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    priv = json.loads(RSAAlgorithm.to_jwk(rsa_key))
    priv["alg"] = "RS256"
    priv["kid"] = "wger-test"
    pub = {k: priv[k] for k in ("kty", "n", "e", "alg", "kid")}

    def b64(doc):
        return base64.urlsafe_b64encode(json.dumps(doc).encode()).decode()

    env = Path("/tmp/wger-harness.env")
    env.write_text(
        "\n".join(
            [
                "DJANGO_DEBUG=True",
                "DJANGO_DB_ENGINE=django.db.backends.postgresql",
                "DJANGO_DB_DATABASE=wger",
                "DJANGO_DB_USER=wger",
                "DJANGO_DB_PASSWORD=wger",
                "DJANGO_DB_HOST=127.0.0.1",
                "DJANGO_DB_PORT=5432",
                "SECRET_KEY=contract-harness",
                "TIME_ZONE=America/Santiago",
                "SITE_URL=http://127.0.0.1:8000",
                "DJANGO_MEDIA_ROOT=/tmp/wger-media",
                "MEDIA_ROOT=/tmp/wger-media",
                "DATABASE_URL=postgres://wger:wger@127.0.0.1:5432/wger",
                f"JWT_PRIVATE_KEY={b64(priv)}",
                f"JWT_PUBLIC_KEY={b64(pub)}",
                "POWERSYNC_URL=http://127.0.0.1:8000/ps/",
                "WGER_VERSION=2.7.0",
                "MIN_APP_VERSION=2.1.0",
                "MIN_SERVER_VERSION=2.5.0",
                "",
            ]
        )
    )


def wipe():
    WorkoutLog.objects.all().delete()
    WorkoutSession.objects.all().delete()
    Routine.objects.all().delete()
    LogItem.objects.all().delete()
    NutritionPlan.objects.all().delete()
    Ingredient.objects.all().delete()
    Exercise.objects.all().delete()
    ExerciseCategory.objects.all().delete()
    Muscle.objects.all().delete()
    Equipment.objects.all().delete()
    GalleryImage.objects.all().delete()
    User.objects.all().delete()
    Path("/tmp/wger-media").mkdir(parents=True, exist_ok=True)
    (Path("/tmp/wger-media") / "probe.bin").write_bytes(b"0123456789" * 200)


def main():
    keys()
    wipe()
    language, _ = Language.objects.get_or_create(
        short_name="en",
        defaults={"full_name": "English", "full_name_en": "English"},
    )
    Language.objects.get_or_create(
        short_name="de",
        defaults={"full_name": "Deutsch", "full_name_en": "German"},
    )
    license_obj, _ = License.objects.get_or_create(
        short_name="CC-BY-SA 4",
        defaults={
            "full_name": "Creative Commons Attribution Share Alike 4",
            "url": "https://creativecommons.org/licenses/by-sa/4.0/deed.en",
        },
    )
    gym = Gym.objects.create(name="Heavenly")
    user = User.objects.create_user(username="admin", email="admin@example.com", password="unused")
    user.password = PBKDF2
    user.is_superuser = False
    user.is_staff = False
    user.date_joined = timezone.make_aware(datetime(2013, 4, 27, 14, 52, 38))
    user.save()
    EmailAddress.objects.create(user=user, email="admin@example.com", primary=True, verified=False)
    Token.objects.create(user=user, key=TOKEN)
    profile = user.userprofile
    profile.gym = gym
    profile.time_zone = "America/Santiago"
    profile.weight_unit = "kg"
    profile.notification_language = language
    profile.age = 31
    profile.birthdate = date(1995, 1, 23)
    profile.height = 190
    profile.gender = "1"
    profile.sleep_hours = 7
    profile.work_hours = 8
    profile.work_intensity = "1"
    profile.sport_hours = 3
    profile.sport_intensity = "2"
    profile.freetime_hours = 8
    profile.freetime_intensity = "1"
    profile.calories = 2500
    profile.workout_reminder = 14
    profile.workout_duration = 12
    profile.save()

    category = ExerciseCategory.objects.create(name="Chest")
    muscle = Muscle.objects.create(name="Pectoralis major", name_en="Chest", is_front=True)
    equipment = Equipment.objects.create(name="Barbell")
    bench = Exercise.objects.create(
        category=category,
        license=license_obj,
        license_author="wger",
    )
    bench.muscles.add(muscle)
    bench.equipment.add(equipment)
    bench_tr = Translation.objects.create(
        exercise=bench,
        language=language,
        name="Bench press",
        description_source=DESC,
        license=license_obj,
        license_author="wger",
    )
    Alias.objects.create(translation=bench_tr, alias="Barbell bench press")
    ExerciseComment.objects.create(translation=bench_tr, comment="Keep the wrists stacked.")
    incline = Exercise.objects.create(category=category, license=license_obj, license_author="wger")
    Translation.objects.create(
        exercise=incline,
        language=language,
        name="Incline bench press",
        description_source=DESC,
        license=license_obj,
        license_author="wger",
    )
    image_id = None
    try:
        image = ExerciseImage(exercise=bench, is_main=True, style=ExerciseImage.PHOTO, license=license_obj)
        image.image.save("bench.png", ContentFile(PNG), save=True)
        image_id = image.id
    except Exception as exc:  # noqa: BLE001
        print("exercise image skipped:", exc)

    banana = Ingredient.objects.create(
        language=language,
        license=license_obj,
        name="Banana",
        energy=89,
        protein=Decimal("1.090"),
        carbohydrates=Decimal("22.840"),
        carbohydrates_sugar=Decimal("12.230"),
        fat=Decimal("0.330"),
        fat_saturated=Decimal("0.110"),
        fiber=Decimal("2.600"),
        sodium=Decimal("0.001"),
        license_author="usda",
    )
    Ingredient.objects.create(
        language=language,
        license=license_obj,
        name="Green banana",
        energy=90,
        protein=Decimal("1.000"),
        carbohydrates=Decimal("22.000"),
        fat=Decimal("0.200"),
        license_author="usda",
    )
    barcode = Ingredient.objects.create(
        language=language,
        license=license_obj,
        name="Packaged oats",
        code="9300633749033",
        energy=370,
        protein=Decimal("13.000"),
        carbohydrates=Decimal("60.000"),
        fat=Decimal("7.000"),
        license_author="off",
    )
    unit = IngredientWeightUnit.objects.create(ingredient=banana, gram=120, name="medium")

    routine = Routine.objects.create(
        user=user,
        name="Push",
        description="Upper push",
        start=date(2024, 1, 1),
        end=date(2024, 1, 14),
        fit_in_week=True,
    )
    Label.objects.create(routine=routine, start_offset=0, end_offset=6, label="week 1")
    day = Day.objects.create(routine=routine, order=1, name="Push A", description="pressing", type="custom")
    slot = Slot.objects.create(day=day, order=1, comment="main")
    entry = SlotEntry.objects.create(
        slot=slot,
        exercise=bench,
        order=1,
        comment="controlled",
        type="normal",
        repetition_unit_id=1,
        weight_unit_id=1,
    )
    other = User.objects.create_user(username="other", email="other@example.com", password="unused")
    other.password = PBKDF2
    other.save()
    other_routine = Routine.objects.create(
        user=other,
        name="Other routine",
        start=date(2024, 3, 1),
        end=date(2024, 3, 14),
        fit_in_week=True,
    )
    other_day = Day.objects.create(routine=other_routine, order=1, name="Other day", type="custom")
    other_slot = Slot.objects.create(day=other_day, order=1)
    foreign_entry = SlotEntry.objects.create(
        slot=other_slot,
        exercise=bench,
        order=1,
        type="normal",
        repetition_unit_id=1,
        weight_unit_id=1,
    )
    sets = SetsConfig.objects.create(slot_entry=entry, iteration=1, value=3)
    reps = RepetitionsConfig.objects.create(slot_entry=entry, iteration=1, value=Decimal("8.00"))
    weight = WeightConfig.objects.create(slot_entry=entry, iteration=1, value=Decimal("40.00"))
    rest = RestConfig.objects.create(slot_entry=entry, iteration=1, value=90)
    rir = RiRConfig.objects.create(slot_entry=entry, iteration=1, value=Decimal("2.0"))
    max_sets = MaxSetsConfig.objects.create(slot_entry=entry, iteration=1, value=4)
    max_reps = MaxRepetitionsConfig.objects.create(slot_entry=entry, iteration=1, value=Decimal("12.00"))
    max_weight = MaxWeightConfig.objects.create(slot_entry=entry, iteration=1, value=Decimal("60.00"))
    max_rest = MaxRestConfig.objects.create(slot_entry=entry, iteration=1, value=120)
    max_rir = MaxRiRConfig.objects.create(slot_entry=entry, iteration=1, value=Decimal("3.0"))
    template = Routine.objects.create(
        user=user,
        name="Template",
        description="public",
        start=date(2024, 2, 1),
        end=date(2024, 2, 14),
        fit_in_week=False,
        is_template=True,
        is_public=True,
    )
    started = timezone.make_aware(datetime(2024, 1, 2, 18, 0, 0))
    session = WorkoutSession.objects.create(
        user=user,
        routine=routine,
        day=day,
        datetime_start=started,
        datetime_end=timezone.make_aware(datetime(2024, 1, 2, 19, 0, 0)),
        notes="solid",
        impression="3",
    )
    log = WorkoutLog.objects.create(
        user=user,
        date=started,
        session=session,
        exercise=bench,
        routine=routine,
        slot_entry=entry,
        iteration=1,
        repetitions=Decimal("8.00"),
        weight=Decimal("40.00"),
        rir=Decimal("2.0"),
        rest=90,
        repetitions_unit_id=1,
        weight_unit_id=1,
    )
    plan = NutritionPlan.objects.create(user=user, description="Cut", start=date(2024, 1, 1))
    meal = Meal.objects.create(plan=plan, order=1, time="08:00", name="Breakfast")
    item = MealItem.objects.create(meal=meal, ingredient=banana, weight_unit=unit, order=1, amount=Decimal("2.00"))
    diary = LogItem.objects.create(
        plan=plan,
        meal=meal,
        ingredient=banana,
        weight_unit=unit,
        amount=Decimal("1.00"),
        datetime=timezone.make_aware(datetime(2024, 1, 3, 8, 30, 0)),
    )
    body = Category.get_or_create_body_weight(user, unit="kg")
    weight_entry = Measurement.objects.create(
        category=body,
        date=timezone.make_aware(datetime(2024, 1, 4, 7, 0, 0)),
        value=Decimal("80.50"),
    )
    gallery_id = None
    try:
        gallery = GalleryImage(user=user, description="front", date=date.today())
        gallery.image.save("front.png", ContentFile(PNG), save=True)
        gallery_id = gallery.id
    except Exception as exc:  # noqa: BLE001
        print("gallery skipped:", exc)

    ids = {
        "token": TOKEN,
        "username": "admin",
        "email": "admin@example.com",
        "password": "s3cret-pass",
        "user_id": user.id,
        "language_id": language.id,
        "license_id": license_obj.id,
        "category_id": category.id,
        "muscle_id": muscle.id,
        "equipment_id": equipment.id,
        "exercise_id": bench.id,
        "exercise_name": "Bench press",
        "translation_id": bench_tr.id,
        "other_exercise_id": incline.id,
        "image_id": image_id,
        "ingredient_id": banana.id,
        "ingredient_name": "Banana",
        "barcode_ingredient_id": barcode.id,
        "barcode": "9300633749033",
        "weight_unit_id": unit.id,
        "routine_id": routine.id,
        "template_id": template.id,
        "day_id": day.id,
        "slot_id": slot.id,
        "entry_id": entry.id,
        "foreign_entry_id": foreign_entry.id,
        "sets_id": sets.id,
        "reps_id": reps.id,
        "weight_config_id": weight.id,
        "rest_id": rest.id,
        "rir_id": rir.id,
        "max_sets_id": max_sets.id,
        "max_reps_id": max_reps.id,
        "max_weight_id": max_weight.id,
        "max_rest_id": max_rest.id,
        "max_rir_id": max_rir.id,
        "session_id": str(session.id),
        "log_id": str(log.id),
        "plan_id": str(plan.id),
        "meal_id": str(meal.id),
        "mealitem_id": str(item.id),
        "diary_id": str(diary.id),
        "measurement_category_id": str(body.id),
        "weight_id": str(weight_entry.id),
        "gallery_id": gallery_id,
    }
    Path("/tmp/wger-harness-ids.json").write_text(json.dumps(ids, indent=2))
    print(json.dumps(ids, indent=2))


if __name__ == "__main__":
    main()
